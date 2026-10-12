use crate::{
    ignore::IgnoreMatcher,
    models::{EntryKind, FileEntry, ScanProgress},
};
use chrono::{DateTime, Utc};
use md5::{Digest, Md5};
use std::{
    fs::{self, File, Metadata},
    io::Read,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use walkdir::WalkDir;

pub const SCAN_CANCELLED: &str = "扫描已取消";

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
        if last_report.elapsed() >= Duration::from_millis(150) {
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

fn md5_reader(
    mut reader: impl Read,
    cancel: &Arc<AtomicBool>,
    mut report: impl FnMut(u64),
) -> Result<(String, u64), String> {
    let mut digest = Md5::new();
    let mut buffer = vec![0_u8; 256 * 1024];
    let mut processed = 0;
    loop {
        cancelled(cancel)?;
        let read = reader
            .read(&mut buffer)
            .map_err(|e| format!("读取文件失败: {e}"))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        processed += read as u64;
        report(processed);
    }
    cancelled(cancel)?;
    Ok((format!("{:x}", digest.finalize()), processed))
}

fn stable_file(
    path: &Path,
    relative: &str,
    token: &Arc<AtomicBool>,
    progress: &ScanProgress,
    last_report: &mut Instant,
    report: &mut impl FnMut(ScanProgress),
) -> Result<FileEntry, String> {
    // Retry once if a writer changes a file while hashing. Never record a mixed read silently.
    for attempt in 0..2 {
        cancelled(token)?;
        let path_before = fs::symlink_metadata(path).map_err(|e| format!("{relative}: {e}"))?;
        if is_link(&path_before) || !path_before.is_file() {
            return Err(format!("文件类型在扫描期间变化: {relative}，请重新扫描"));
        }
        let file = File::open(path).map_err(|e| format!("无法打开 {relative}: {e}"))?;
        let before = file.metadata().map_err(|e| format!("{relative}: {e}"))?;
        let time = modified_time(&before)?;
        let (md5, bytes) = md5_reader(&file, token, |bytes| {
            if last_report.elapsed() >= Duration::from_millis(150) {
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
                md5,
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
    let total = paths.len();
    let total_bytes = paths
        .iter()
        .try_fold(0u64, |acc, p| acc.checked_add(p.2))
        .ok_or("总大小超出范围")?;
    let mut processed_bytes = 0;
    let mut entries = Vec::with_capacity(total);
    let mut last_report = Instant::now();
    for (index, (path, kind, size)) in paths.iter().enumerate() {
        cancelled(token)?;
        let relative = relative_path(root, path)?;
        let progress = ScanProgress {
            phase: if *kind == EntryKind::File {
                "hashing"
            } else {
                "scanning"
            }
            .into(),
            processed: index,
            total,
            current_path: relative.clone(),
            current_file_size: *size,
            processed_bytes,
            total_bytes,
            ..Default::default()
        };
        if index == 0 || last_report.elapsed() >= Duration::from_millis(150) {
            report(progress.clone());
            last_report = Instant::now();
        }
        let entry = match kind {
            EntryKind::File => stable_file(
                path,
                &relative,
                token,
                &progress,
                &mut last_report,
                &mut report,
            )?,
            EntryKind::Directory => {
                let metadata =
                    fs::symlink_metadata(path).map_err(|e| format!("{relative}: {e}"))?;
                if !metadata.is_dir() || is_link(&metadata) {
                    return Err(format!("目录类型发生变化: {relative}"));
                }
                FileEntry {
                    relative_path: relative,
                    kind: *kind,
                    size: 0,
                    modified_time: modified_time(&metadata)?,
                    md5: String::new(),
                }
            }
            EntryKind::Symlink => {
                let metadata =
                    fs::symlink_metadata(path).map_err(|e| format!("{relative}: {e}"))?;
                let target =
                    fs::read_link(path).map_err(|e| format!("无法读取链接 {relative}: {e}"))?;
                let target = target
                    .to_str()
                    .ok_or_else(|| format!("链接目标不是有效 Unicode: {relative}"))?;
                // Hash the link target path, never follow the target or save its contents.
                FileEntry {
                    relative_path: relative,
                    kind: *kind,
                    size: 0,
                    modified_time: modified_time(&metadata)?,
                    md5: format!("{:x}", Md5::digest(target.as_bytes())),
                }
            }
        };
        processed_bytes += entry.size;
        // Many tiny files must not flood the WebView IPC queue. Cancellation is
        // still checked for every entry and hash chunk, independently of events.
        if index + 1 == total || last_report.elapsed() >= Duration::from_millis(150) {
            report(ScanProgress {
                phase: "scanning".into(),
                processed: index + 1,
                current_file_bytes: entry.size,
                processed_bytes,
                ..progress
            });
            last_report = Instant::now();
        }
        entries.push(entry);
    }
    entries.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    cancelled(token)?;
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    #[test]
    fn streamed_hash_and_cancellation_between_chunks() {
        let cancel = Arc::new(AtomicBool::new(false));
        assert_eq!(
            md5_reader(Cursor::new(b"abc"), &cancel, |_| {}).unwrap().0,
            "900150983cd24fb0d6963f7d28e17f72"
        );
        let mut bytes = 0;
        let result = md5_reader(Cursor::new(vec![7; 2 * 1024 * 1024]), &cancel, |n| {
            bytes = n;
            cancel.store(true, Ordering::Relaxed);
        });
        assert_eq!(result.unwrap_err(), SCAN_CANCELLED);
        assert_eq!(bytes, 256 * 1024);
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
            &token,
            &ScanProgress::default(),
            &mut last,
            &mut |_| {
                if !changed {
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
            entry.md5,
            format!("{:x}", Md5::digest(fs::read(path).unwrap()))
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
}
