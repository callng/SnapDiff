use crate::{
    ignore::IgnoreMatcher,
    models::{EntryKind, FileEntry, ScanProgress},
};
use chrono::{DateTime, Utc};

use std::{
    fs::{self, File, Metadata},
    io::Read,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, SyncSender},
        Arc, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};
use walkdir::WalkDir;

pub const SCAN_CANCELLED: &str = "扫描已取消";
const PROGRESS_INTERVAL: Duration = Duration::from_millis(150);
// More readers can hurt rotational/network storage. CPU count is an upper
// bound, not a reason to open one file per logical processor on large hosts.
const MAX_HASH_WORKERS: usize = 8;
const READ_CHUNK: usize = 256 * 1024;
const HASH_BATCH: usize = 4 * 1024 * 1024;
const PARALLEL_HASH_THRESHOLD: usize = 1024 * 1024;

struct ScanControl<'a> {
    requested: &'a AtomicBool,
    stopped: AtomicBool,
    hash_pool: OnceLock<Result<rayon::ThreadPool, String>>,
    hash_threads: usize,
}

impl<'a> ScanControl<'a> {
    fn new(requested: &'a AtomicBool) -> Self {
        Self {
            requested,
            stopped: AtomicBool::new(false),
            hash_pool: OnceLock::new(),
            hash_threads: thread::available_parallelism().map_or(1, usize::from),
        }
    }

    fn check(&self) -> Result<(), String> {
        if self.requested.load(Ordering::Relaxed) || self.stopped.load(Ordering::Relaxed) {
            Err(SCAN_CANCELLED.into())
        } else {
            Ok(())
        }
    }
}

fn hash_worker_count(logical_cpus: usize, files: usize) -> usize {
    logical_cpus.clamp(1, MAX_HASH_WORKERS).min(files.max(1))
}

fn cancelled(token: &Arc<AtomicBool>) -> Result<(), String> {
    if token.load(Ordering::Relaxed) {
        Err(SCAN_CANCELLED.into())
    } else {
        Ok(())
    }
}

fn relative_path(root: &Path, path: &Path) -> Result<String, String> {
    let relative = path.strip_prefix(root).map_err(|e| e.to_string())?;
    relative
        .to_str()
        .map(|p| p.replace('\\', "/"))
        .ok_or_else(|| format!("路径不是有效 Unicode，无法完整记录: {}", path.display()))
}

fn is_link(metadata: &Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    false
}

fn collect_entries(
    root: &Path,
    ignore: &IgnoreMatcher,
    token: &Arc<AtomicBool>,
    report: &mut impl FnMut(ScanProgress),
) -> Result<Vec<(PathBuf, EntryKind, u64)>, String> {
    let mut result = Vec::new();
    let mut error = None;
    let mut last_report = Instant::now();
    let mut walker = WalkDir::new(root).follow_links(false).into_iter();
    while let Some(item) = walker.next() {
        cancelled(token)?;
        let entry = item.map_err(|e| format!("目录读取失败，未保存本次状态: {e}"))?;
        if entry.path() == root {
            continue;
        }
        let rel = relative_path(root, entry.path())?;
        let metadata =
            fs::symlink_metadata(entry.path()).map_err(|e| format!("无法读取 {rel}: {e}"))?;
        let link = is_link(&metadata);
        let is_dir = metadata.is_dir() && !link;
        if link && entry.file_type().is_dir() {
            walker.skip_current_dir();
        }
        if ignore.is_ignored(&rel, is_dir) {
            if is_dir && !ignore.has_negations {
                walker.skip_current_dir();
            }
            continue;
        }
        let kind = if link {
            EntryKind::Symlink
        } else if is_dir {
            EntryKind::Directory
        } else if metadata.is_file() {
            EntryKind::File
        } else {
            error = Some(format!("不支持的文件类型: {rel}"));
            break;
        };
        result.push((
            entry.into_path(),
            kind,
            if kind == EntryKind::File {
                metadata.len()
            } else {
                0
            },
        ));
        if last_report.elapsed() >= PROGRESS_INTERVAL {
            report(ScanProgress {
                phase: "collecting".into(),
                processed: result.len(),
                current_path: rel,
                ..Default::default()
            });
            last_report = Instant::now();
        }
    }
    if let Some(error) = error {
        return Err(error);
    }
    cancelled(token)?;
    Ok(result)
}

fn modified_time(metadata: &Metadata) -> Result<DateTime<Utc>, String> {
    metadata
        .modified()
        .map(DateTime::<Utc>::from)
        .map_err(|e| format!("无法读取修改时间: {e}"))
}

fn hash_reader(
    mut reader: impl Read,
    cancel: &ScanControl<'_>,
    mut report: impl FnMut(u64),
) -> Result<(String, u64), String> {
    let mut digest = blake3::Hasher::new();
    // Retain cancellable 256 KiB reads, but give BLAKE3 batches large enough
    // to amortize Rayon scheduling and exploit its tree/SIMD implementation.
    let mut buffer = vec![0_u8; READ_CHUNK];
    let mut processed = 0u64;
    loop {
        let mut filled = 0;
        while filled < buffer.len() {
            cancel.check()?;
            let end = (filled + READ_CHUNK).min(buffer.len());
            let read = match reader.read(&mut buffer[filled..end]) {
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(format!("读取文件失败: {error}")),
            };
            if read == 0 {
                break;
            }
            filled += read;
        }
        cancel.check()?;
        if filled == 0 {
            break;
        }
        if filled >= PARALLEL_HASH_THRESHOLD && cancel.hash_threads > 1 {
            // One pool per scan, shared across all readers. Do not create a
            // separate CPU-sized pool for every file or touch global Rayon state.
            let pool = cancel
                .hash_pool
                .get_or_init(|| {
                    rayon::ThreadPoolBuilder::new()
                        .num_threads(cancel.hash_threads)
                        .thread_name(|i| format!("snapdiff-blake3-{i}"))
                        .build()
                        .map_err(|e| format!("无法启动 BLAKE3 工作线程: {e}"))
                })
                .as_ref()
                .map_err(Clone::clone)?;
            pool.install(|| digest.update_rayon(&buffer[..filled]));
        } else {
            digest.update(&buffer[..filled]);
        }
        cancel.check()?;
        processed = processed
            .checked_add(filled as u64)
            .ok_or("文件大小超出范围")?;
        report(processed);
        if filled == buffer.len() && buffer.len() < HASH_BATCH {
            buffer.resize(HASH_BATCH, 0);
        }
    }
    cancel.check()?;
    Ok((digest.finalize().to_hex().to_string(), processed))
}

fn stable_file(
    path: &Path,
    relative: &str,
    token: &ScanControl<'_>,
    progress: &ScanProgress,
    last_report: &mut Instant,
    report: &mut impl FnMut(ScanProgress),
) -> Result<FileEntry, String> {
    // Retry once if a writer changes a file while hashing. Never record a mixed read silently.
    for attempt in 0..2 {
        token.check()?;
        let path_before = fs::symlink_metadata(path).map_err(|e| format!("{relative}: {e}"))?;
        if is_link(&path_before) || !path_before.is_file() {
            return Err(format!("文件类型在扫描期间变化: {relative}，请重新扫描"));
        }
        let file = File::open(path).map_err(|e| format!("无法打开 {relative}: {e}"))?;
        let before = file.metadata().map_err(|e| format!("{relative}: {e}"))?;
        let time = modified_time(&before)?;
        // A retry restarts this file's contribution; never count both reads.
        report(ScanProgress {
            current_file_bytes: 0,
            current_file_size: before.len(),
            ..progress.clone()
        });
        let (hash, bytes) = hash_reader(&file, token, |bytes| {
            if last_report.elapsed() >= PROGRESS_INTERVAL {
                report(ScanProgress {
                    current_file_bytes: bytes,
                    current_file_size: before.len(),
                    ..progress.clone()
                });
                *last_report = Instant::now();
            }
        })
        .map_err(|e| {
            if e == SCAN_CANCELLED {
                e
            } else {
                format!("{relative}: {e}")
            }
        })?;
        let after = file.metadata().map_err(|e| format!("{relative}: {e}"))?;
        let path_after = fs::symlink_metadata(path).map_err(|e| format!("{relative}: {e}"))?;
        if !is_link(&path_after)
            && bytes == before.len()
            && before.len() == after.len()
            && before.modified().ok() == after.modified().ok()
            && path_before.len() == before.len()
            && path_before.modified().ok() == before.modified().ok()
            && after.len() == path_after.len()
            && after.modified().ok() == path_after.modified().ok()
        {
            return Ok(FileEntry {
                relative_path: relative.into(),
                kind: EntryKind::File,
                size: bytes,
                modified_time: time,
                hash,
            });
        }
        if attempt == 1 {
            return Err(format!(
                "文件在扫描期间持续变化: {relative}，未保存本次状态"
            ));
        }
    }
    unreachable!()
}

pub fn scan_directory(
    root: &Path,
    ignore: &IgnoreMatcher,
    token: &Arc<AtomicBool>,
    mut report: impl FnMut(ScanProgress),
) -> Result<Vec<FileEntry>, String> {
    report(ScanProgress {
        phase: "collecting".into(),
        ..Default::default()
    });
    let paths = collect_entries(root, ignore, token, &mut report)?;
    let file_count = paths.iter().filter(|p| p.1 == EntryKind::File).count();
    let workers = hash_worker_count(
        thread::available_parallelism().map_or(1, usize::from),
        file_count,
    );
    scan_entries(root, &paths, &ScanControl::new(token), workers, &mut report)
}

enum WorkerEvent {
    Progress(usize, ScanProgress),
    Entry(usize, usize, Result<FileEntry, String>),
}

fn scan_entry(
    root: &Path,
    path: &Path,
    kind: EntryKind,
    control: &ScanControl<'_>,
    last_report: &mut Instant,
    report: &mut impl FnMut(ScanProgress),
) -> Result<FileEntry, String> {
    control.check()?;
    let relative = relative_path(root, path)?;
    match kind {
        EntryKind::File => stable_file(
            path,
            &relative,
            control,
            &ScanProgress {
                phase: "hashing".into(),
                current_path: relative.clone(),
                ..Default::default()
            },
            last_report,
            report,
        ),
        EntryKind::Directory => {
            let metadata = fs::symlink_metadata(path).map_err(|e| format!("{relative}: {e}"))?;
            if !metadata.is_dir() || is_link(&metadata) {
                return Err(format!("目录类型发生变化: {relative}"));
            }
            Ok(FileEntry {
                relative_path: relative,
                kind,
                size: 0,
                modified_time: modified_time(&metadata)?,
                hash: String::new(),
            })
        }
        EntryKind::Symlink => {
            let metadata = fs::symlink_metadata(path).map_err(|e| format!("{relative}: {e}"))?;
            let target =
                fs::read_link(path).map_err(|e| format!("无法读取链接 {relative}: {e}"))?;
            let target = target
                .to_str()
                .ok_or_else(|| format!("链接目标不是有效 Unicode: {relative}"))?;
            Ok(FileEntry {
                relative_path: relative,
                kind,
                size: 0,
                modified_time: modified_time(&metadata)?,
                // Hash the link path, never follow or persist its target.
                hash: blake3::hash(target.as_bytes()).to_hex().to_string(),
            })
        }
    }
}

fn scan_worker(
    root: &Path,
    paths: &[(PathBuf, EntryKind, u64)],
    control: &ScanControl<'_>,
    next: &AtomicUsize,
    worker: usize,
    sender: SyncSender<WorkerEvent>,
) {
    let mut last_report = Instant::now();
    while control.check().is_ok() {
        let index = next.fetch_add(1, Ordering::Relaxed);
        let Some((path, kind, _)) = paths.get(index) else {
            break;
        };
        let result = scan_entry(root, path, *kind, control, &mut last_report, &mut |p| {
            // The bounded channel also caps progress queued behind the caller.
            let _ = sender.send(WorkerEvent::Progress(worker, p));
        });
        let failed = result.is_err();
        if sender
            .send(WorkerEvent::Entry(worker, index, result))
            .is_err()
            || failed
        {
            break;
        }
    }
}

fn scan_entries(
    root: &Path,
    paths: &[(PathBuf, EntryKind, u64)],
    control: &ScanControl<'_>,
    workers: usize,
    report: &mut impl FnMut(ScanProgress),
) -> Result<Vec<FileEntry>, String> {
    control.check()?;
    let total = paths.len();
    let mut total_bytes = paths
        .iter()
        .try_fold(0u64, |acc, p| acc.checked_add(p.2))
        .ok_or("总大小超出范围")?;
    let mut completed_bytes = 0u64;
    let mut entries = Vec::with_capacity(total);
    let mut last_report = Instant::now();
    if total == 0 {
        report(ScanProgress {
            phase: "scanning".into(),
            ..Default::default()
        });
        control.check()?;
        return Ok(entries);
    }
    let next = AtomicUsize::new(0);
    let mut active_bytes = vec![0u64; workers];
    let mut reported = false;
    thread::scope(|scope| -> Result<(), String> {
        let (sender, receiver) = mpsc::sync_channel(workers * 2);
        let mut handles = Vec::with_capacity(workers);
        let mut failure = None;
        for worker in 0..workers {
            let sender = sender.clone();
            let next = &next;
            match thread::Builder::new()
                .name(format!("snapdiff-hash-{worker}"))
                .spawn_scoped(scope, move || {
                    scan_worker(root, paths, control, next, worker, sender);
                }) {
                Ok(handle) => handles.push(handle),
                Err(error) => {
                    failure = Some(format!("无法启动 BLAKE3 工作线程: {error}"));
                    control.stopped.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
        drop(sender);
        // Only this coordinator invokes the callback: neither the Tauri bridge
        // nor callers/tests need a Send/Sync callback or competing event order.
        for event in &receiver {
            if failure.is_some() {
                break;
            }
            if let Err(error) = control.check() {
                failure = Some(error);
                break;
            }
            let mut progress = match event {
                WorkerEvent::Progress(worker, progress) => {
                    active_bytes[worker] = progress.current_file_bytes;
                    progress
                }
                WorkerEvent::Entry(worker, index, result) => {
                    let entry = match result {
                        Ok(entry) => entry,
                        Err(error) => {
                            failure = Some(error);
                            break;
                        }
                    };
                    active_bytes[worker] = 0;
                    let Some(bytes) = completed_bytes.checked_add(entry.size) else {
                        failure = Some("总大小超出范围".into());
                        break;
                    };
                    completed_bytes = bytes;
                    // A stable retry may capture a different size than enumeration.
                    let Some(bytes) = total_bytes
                        .checked_sub(paths[index].2)
                        .and_then(|n| n.checked_add(entry.size))
                    else {
                        failure = Some("总大小超出范围".into());
                        break;
                    };
                    total_bytes = bytes;
                    let progress = ScanProgress {
                        phase: "scanning".into(),
                        current_path: entry.relative_path.clone(),
                        current_file_size: entry.size,
                        current_file_bytes: entry.size,
                        ..Default::default()
                    };
                    entries.push(entry);
                    progress
                }
            };
            let Some(bytes) = active_bytes
                .iter()
                .try_fold(completed_bytes, |sum, n| sum.checked_add(*n))
            else {
                failure = Some("总大小超出范围".into());
                break;
            };
            // Existing consumers add current_file_bytes during hashing. Include
            // every other active reader here, without counting the displayed one twice.
            progress.processed_bytes = bytes
                - if progress.phase == "hashing" {
                    progress.current_file_bytes
                } else {
                    0
                };
            progress.total_bytes = total_bytes;
            progress.processed = entries.len();
            progress.total = total;
            if !reported || entries.len() == total || last_report.elapsed() >= PROGRESS_INTERVAL {
                report(progress);
                reported = true;
                last_report = Instant::now();
            }
        }
        if failure.is_some() || control.check().is_err() {
            control.stopped.store(true, Ordering::Relaxed);
        }
        // Disconnect before joining, so blocked senders can always exit.
        drop(receiver);
        for handle in handles {
            if handle.join().is_err() && failure.is_none() {
                failure = Some("BLAKE3 工作线程异常退出，未保存本次状态".into());
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        control.check()?;
        if entries.len() != total {
            return Err("扫描未完成，未保存本次状态".into());
        }
        Ok(())
    })?;
    entries.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    control.check()?;
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    #[test]
    fn streamed_hash_and_cancellation_between_chunks() {
        let cancel = Arc::new(AtomicBool::new(false));
        let control = ScanControl::new(&cancel);
        assert_eq!(
            hash_reader(Cursor::new(b""), &control, |_| {}).unwrap().0,
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
        assert_eq!(
            hash_reader(Cursor::new(b"abc"), &control, |_| {})
                .unwrap()
                .0,
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
        );
        let mut bytes = 0;
        let result = hash_reader(Cursor::new(vec![7; 2 * HASH_BATCH]), &control, |n| {
            bytes = n;
            cancel.store(true, Ordering::Relaxed);
        });
        assert_eq!(result.unwrap_err(), SCAN_CANCELLED);
        assert_eq!(bytes, READ_CHUNK as u64);
    }
    #[test]
    fn blake3_parallel_batches_match_official_hash_across_boundaries() {
        struct ShortReader<'a> {
            bytes: &'a [u8],
            interrupt: bool,
        }
        impl Read for ShortReader<'_> {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                if self.interrupt {
                    self.interrupt = false;
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                let count = out.len().min(self.bytes.len()).min(7777);
                out[..count].copy_from_slice(&self.bytes[..count]);
                self.bytes = &self.bytes[count..];
                Ok(count)
            }
        }
        let bytes: Vec<u8> = (0..(2 * HASH_BATCH + READ_CHUNK + 17))
            .map(|i| (i % 251) as u8)
            .collect();
        let token = AtomicBool::new(false);
        let mut control = ScanControl::new(&token);
        control.hash_threads = 2;
        for len in [
            0,
            1,
            1023,
            1024,
            READ_CHUNK - 1,
            READ_CHUNK,
            READ_CHUNK + 1,
            PARALLEL_HASH_THRESHOLD,
            HASH_BATCH + READ_CHUNK,
            bytes.len(),
        ] {
            let (hash, read) = hash_reader(
                ShortReader {
                    bytes: &bytes[..len],
                    interrupt: true,
                },
                &control,
                |_| {},
            )
            .unwrap();
            assert_eq!(
                hash,
                blake3::hash(&bytes[..len]).to_hex().to_string(),
                "length={len}"
            );
            assert_eq!(read, len as u64);
        }
        assert_eq!(
            control
                .hash_pool
                .get()
                .unwrap()
                .as_ref()
                .unwrap()
                .current_num_threads(),
            2
        );
    }

    #[test]
    fn blake3_cancels_between_reads_and_propagates_read_errors() {
        struct CancellingRead<'a> {
            token: &'a AtomicBool,
            count: usize,
        }
        impl Read for CancellingRead<'_> {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                out.fill(5);
                self.count += 1;
                if self.count == 2 {
                    self.token.store(true, Ordering::Relaxed);
                }
                Ok(out.len())
            }
        }
        let token = AtomicBool::new(false);
        let mut reader = CancellingRead {
            token: &token,
            count: 0,
        };
        assert_eq!(
            hash_reader(&mut reader, &ScanControl::new(&token), |_| {}).unwrap_err(),
            SCAN_CANCELLED
        );
        assert_eq!(reader.count, 2);
        token.store(false, Ordering::Relaxed);
        struct FailingRead;
        impl Read for FailingRead {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("injected read failure"))
            }
        }
        assert!(hash_reader(FailingRead, &ScanControl::new(&token), |_| {})
            .unwrap_err()
            .contains("injected read failure"));
    }

    #[test]
    fn file_changed_during_hash_is_retried_with_fresh_metadata() {
        use std::io::Write;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("changing.bin");
        let original = vec![7u8; 1024 * 1024];
        fs::write(&path, &original).unwrap();
        let token = Arc::new(AtomicBool::new(false));
        let mut last = Instant::now() - Duration::from_secs(1);
        let mut changed = false;
        let entry = stable_file(
            &path,
            "changing.bin",
            &ScanControl::new(&token),
            &ScanProgress::default(),
            &mut last,
            &mut |p| {
                if !changed && p.current_file_bytes > 0 {
                    fs::OpenOptions::new()
                        .append(true)
                        .open(&path)
                        .unwrap()
                        .write_all(b"x")
                        .unwrap();
                    changed = true;
                }
            },
        )
        .unwrap();
        assert!(changed);
        assert_eq!(entry.size, original.len() as u64 + 1);
        assert_eq!(
            entry.hash,
            blake3::hash(&fs::read(path).unwrap()).to_hex().to_string()
        );
    }
    #[test]
    fn empty_directories_and_ignore_exceptions_are_captured() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("empty")).unwrap();
        fs::create_dir_all(root.path().join("cache")).unwrap();
        fs::write(root.path().join("cache/keep.txt"), b"abc").unwrap();
        fs::write(root.path().join("cache/drop.txt"), b"def").unwrap();
        let matcher =
            IgnoreMatcher::from_patterns(&["cache/".into(), "!cache/keep.txt".into()]).unwrap();
        let entries = scan_directory(
            root.path(),
            &matcher,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        assert!(entries
            .iter()
            .any(|e| e.relative_path == "empty" && e.kind == EntryKind::Directory));
        assert!(entries.iter().any(|e| e.relative_path == "cache/keep.txt"));
        assert!(!entries.iter().any(|e| e.relative_path == "cache/drop.txt"));
    }
    #[test]
    fn cancelled_scan_does_not_return_a_partial_state() {
        let root = tempfile::tempdir().unwrap();
        let matcher = IgnoreMatcher::from_root(root.path(), &[]).unwrap();
        assert_eq!(
            scan_directory(
                root.path(),
                &matcher,
                &Arc::new(AtomicBool::new(true)),
                |_| {}
            )
            .unwrap_err(),
            SCAN_CANCELLED
        );
    }

    #[test]
    fn concurrency_is_limited_by_cpus_files_and_io_cap() {
        assert_eq!(hash_worker_count(0, 0), 1);
        assert_eq!(hash_worker_count(1, 100), 1);
        assert_eq!(hash_worker_count(4, 100), 4);
        assert_eq!(hash_worker_count(128, 3), 3);
        assert_eq!(hash_worker_count(128, 100), MAX_HASH_WORKERS);
    }

    #[test]
    fn parallel_scan_matches_serial_hashes_metadata_and_order() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("empty")).unwrap();
        // Uneven lengths and empty files exercise completion out of order.
        for i in 0..32 {
            fs::write(
                root.path().join(format!("文件-{i:02}.bin")),
                vec![i as u8; i * 19321],
            )
            .unwrap();
        }
        let token = Arc::new(AtomicBool::new(false));
        let matcher = IgnoreMatcher::from_patterns(&[]).unwrap();
        let paths = collect_entries(root.path(), &matcher, &token, &mut |_| {}).unwrap();
        let serial = scan_entries(
            root.path(),
            &paths,
            &ScanControl::new(&token),
            1,
            &mut |_| {},
        )
        .unwrap();
        let caller = thread::current().id();
        let mut progress = Vec::new();
        let parallel = scan_entries(
            root.path(),
            &paths,
            &ScanControl::new(&token),
            4,
            &mut |p| {
                assert_eq!(thread::current().id(), caller);
                progress.push(p);
            },
        )
        .unwrap();
        assert_eq!(serial, parallel);
        for entry in parallel.iter().filter(|e| e.kind == EntryKind::File) {
            assert_eq!(
                entry.hash,
                blake3::hash(&fs::read(root.path().join(&entry.relative_path)).unwrap())
                    .to_hex()
                    .to_string()
            );
        }
        let last = progress.last().unwrap();
        assert_eq!(last.processed, parallel.len());
        assert_eq!(
            last.processed_bytes,
            parallel.iter().map(|e| e.size).sum::<u64>()
        );
        assert_eq!(last.processed_bytes, last.total_bytes);
        let mut previous = 0;
        for p in progress {
            let bytes = p.processed_bytes
                + if p.phase == "hashing" {
                    p.current_file_bytes
                } else {
                    0
                };
            assert!(bytes >= previous && bytes <= p.total_bytes);
            previous = bytes;
        }
    }

    #[test]
    fn parallel_cancellation_disconnects_blocked_senders_and_joins_workers() {
        let root = tempfile::tempdir().unwrap();
        for i in 0..16 {
            fs::write(
                root.path().join(format!("{i}.bin")),
                vec![i as u8; 1024 * 1024],
            )
            .unwrap();
        }
        let token = Arc::new(AtomicBool::new(false));
        let matcher = IgnoreMatcher::from_patterns(&[]).unwrap();
        let paths = collect_entries(root.path(), &matcher, &token, &mut |_| {}).unwrap();
        let mut callbacks = 0;
        let result = scan_entries(
            root.path(),
            &paths,
            &ScanControl::new(&token),
            4,
            &mut |_| {
                callbacks += 1;
                // Give other workers time to fill the bounded channel before cancel.
                thread::sleep(Duration::from_millis(20));
                token.store(true, Ordering::Relaxed);
            },
        );
        assert_eq!(result.unwrap_err(), SCAN_CANCELLED);
        assert_eq!(callbacks, 1);
    }

    #[test]
    fn parallel_error_returns_original_path_without_poisoning_cancel_token() {
        let root = tempfile::tempdir().unwrap();
        let token = Arc::new(AtomicBool::new(false));
        let paths: Vec<_> = (0..8)
            .map(|i| (root.path().join(format!("missing-{i}")), EntryKind::File, 1))
            .collect();
        let error = scan_entries(
            root.path(),
            &paths,
            &ScanControl::new(&token),
            4,
            &mut |_| {},
        )
        .unwrap_err();
        assert!(error.contains("missing-"));
        assert!(!token.load(Ordering::Relaxed));
        // A failure must not leave thread/task ownership behind.
        assert!(
            scan_entries(root.path(), &[], &ScanControl::new(&token), 4, &mut |_| {})
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn file_that_keeps_changing_fails_after_one_retry() {
        use std::io::Write;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("changing.bin");
        fs::write(&path, b"abc").unwrap();
        let token = AtomicBool::new(false);
        let mut attempts = 0;
        let result = stable_file(
            &path,
            "changing.bin",
            &ScanControl::new(&token),
            &ScanProgress::default(),
            &mut Instant::now(),
            &mut |_| {
                attempts += 1;
                fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap()
                    .write_all(b"x")
                    .unwrap();
            },
        );
        assert_eq!(attempts, 2);
        assert!(result.unwrap_err().contains("持续变化"));
    }

    #[test]
    fn size_change_since_enumeration_updates_total_without_double_counting() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("growing.bin");
        fs::write(&path, b"abc").unwrap();
        let paths = vec![(path.clone(), EntryKind::File, 3)];
        fs::write(&path, b"abcdef").unwrap();
        let token = AtomicBool::new(false);
        let mut last = ScanProgress::default();
        let entries = scan_entries(
            root.path(),
            &paths,
            &ScanControl::new(&token),
            1,
            &mut |p| {
                last = p;
            },
        )
        .unwrap();
        assert_eq!(
            entries[0].hash,
            blake3::hash(b"abcdef").to_hex().to_string()
        );
        assert_eq!(last.processed_bytes, 6);
        assert_eq!(last.total_bytes, 6);
    }

    #[test]
    #[ignore = "writes 512 MiB of temporary data; run with --release --ignored --nocapture --test-threads=1"]
    fn benchmark_parallel_hashing() {
        use std::io::Write;
        let root = tempfile::tempdir().unwrap();
        let block: Vec<u8> = (0..1024 * 1024).map(|i| (i * 31 + i / 7) as u8).collect();
        for i in 0..8 {
            let mut file = File::create(root.path().join(format!("{i}.bin"))).unwrap();
            for _ in 0..64 {
                file.write_all(&block).unwrap();
            }
            file.sync_all().unwrap();
        }
        let token = Arc::new(AtomicBool::new(false));
        let matcher = IgnoreMatcher::from_patterns(&[]).unwrap();
        let paths = collect_entries(root.path(), &matcher, &token, &mut |_| {}).unwrap();
        let cpus = thread::available_parallelism().map_or(1, usize::from);
        let automatic = hash_worker_count(cpus, 8);
        // Warm OS cache explicitly. Alternating order reduces warm-cache bias.
        scan_entries(
            root.path(),
            &paths,
            &ScanControl::new(&token),
            1,
            &mut |_| {},
        )
        .unwrap();
        eprintln!(
            "fixture=8x64MiB warm_cache=true logical_cpus={cpus} automatic_workers={automatic}"
        );
        for (subset, mib) in [(&paths[..1], 64.0), (&paths[..], 512.0)] {
            let mut expected = None;
            for (workers, hash_threads) in [
                (1, 1),
                (1, cpus),
                (automatic, cpus),
                (automatic, cpus),
                (1, cpus),
                (1, 1),
            ] {
                let workers = workers.min(subset.len());
                let mut control = ScanControl::new(&token);
                control.hash_threads = hash_threads;
                let started = Instant::now();
                let result =
                    scan_entries(root.path(), subset, &control, workers, &mut |_| {}).unwrap();
                let seconds = started.elapsed().as_secs_f64();
                eprintln!(
                "files={} readers={workers} hash_threads={hash_threads} hashing_seconds={seconds:.3} mib_per_second={:.1}",
                subset.len(), mib / seconds
            );
                if let Some(expected) = &expected {
                    assert_eq!(expected, &result);
                } else {
                    expected = Some(result);
                }
            }
        }
    }
}
