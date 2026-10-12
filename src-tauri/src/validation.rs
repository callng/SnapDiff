//! Opt-in real filesystem acceptance and performance checks. Never use user data.
use crate::{diff, ignore::IgnoreMatcher, models::*, scanner, storage};
use std::{
    fs::{self, File},
    io::Write,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

fn scan(root: &Path) -> Result<Vec<FileEntry>, String> {
    scanner::scan_directory(
        root,
        &IgnoreMatcher::from_root(root, &[])?,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
}

fn snapshot(root: &Path, id: &str, files: Vec<FileEntry>) -> Snapshot {
    let mut node = diff::test_snapshot(id, files);
    node.root_path = root.to_str().unwrap().into();
    node
}

#[test]
fn unicode_and_long_windows_paths_preserve_relative_names() {
    let temp = tempfile::tempdir().unwrap();
    let mut folder = fs::canonicalize(temp.path()).unwrap();
    for i in 0..8 {
        folder.push(format!("目录-{i}-{}", "x".repeat(32)));
    }
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("中文 😀 & ' [文件].txt"), b"abc").unwrap();
    let entries = scan(temp.path()).unwrap();
    let file = entries.iter().find(|e| e.kind == EntryKind::File).unwrap();
    assert!(file.relative_path.ends_with("/中文 😀 & ' [文件].txt"));
    assert!(!file.relative_path.contains('\\'));
    assert_eq!(file.md5, "900150983cd24fb0d6963f7d28e17f72");
    assert!(file.relative_path.len() > 260);
}

#[cfg(windows)]
#[test]
fn locked_file_fails_without_committing_or_pruning_history() {
    use std::os::windows::fs::OpenOptionsExt;
    let root = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let path = root.path().join("locked.bin");
    fs::write(&path, b"do not lose history").unwrap();
    let mut store = storage::Store::open(data.path()).unwrap();
    let mut config = storage::new_config(root.path().to_str().unwrap(), &[]);
    config.retention = 1;
    let mut node = snapshot(root.path(), "baseline", scan(root.path()).unwrap());
    node.directory_id = config.id.clone();
    store.commit_scan(&config, &node, true, 0, true).unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&path)
        .unwrap();
    assert!(scan(root.path()).unwrap_err().contains("locked.bin"));
    assert_eq!(store.history(&config.id).unwrap().len(), 1);
    assert_eq!(store.snapshot("baseline").unwrap().files, node.files);
    drop(lock);
    assert!(scan(root.path()).is_ok());
}

#[test]
fn missing_root_is_an_error_instead_of_an_empty_snapshot() {
    let root = tempfile::tempdir().unwrap();
    assert!(scan(&root.path().join("missing")).is_err());
}

#[cfg(windows)]
#[test]
fn junction_is_recorded_without_reading_the_target_tree() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(
        outside.path().join("outside-secret.txt"),
        b"not in snapshot",
    )
    .unwrap();
    let junction = root.path().join("junction");
    // Pass paths through the child environment, never interpolate shell code.
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", "$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path $env:SNAPDIFF_TEST_LINK -Target $env:SNAPDIFF_TEST_TARGET | Out-Null"])
        .env("SNAPDIFF_TEST_LINK", &junction).env("SNAPDIFF_TEST_TARGET", outside.path())
        .output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let entries = scan(root.path()).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].relative_path, "junction");
    assert_eq!(entries[0].kind, EntryKind::Symlink);
    assert_eq!(entries[0].md5.len(), 32);
    assert_eq!(
        storage::root_key(junction.to_str().unwrap()),
        storage::root_key(outside.path().to_str().unwrap())
    );
    // Remove only the junction itself; do not recurse into the external target.
    fs::remove_dir(junction).unwrap();
    assert!(outside.path().join("outside-secret.txt").exists());
}

fn env_count(name: &str, fallback: u64, max: u64) -> u64 {
    let count = std::env::var(name)
        .map(|s| s.parse::<u64>().expect("positive integer"))
        .unwrap_or(fallback);
    assert!(count > 0 && count <= max, "{name} must be in 1..={max}");
    count
}

fn log_peak_memory() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::{
            ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
            Threading::GetCurrentProcess,
        };
        let mut counters = PROCESS_MEMORY_COUNTERS::default();
        let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        counters.cb = size;
        // The current-process pseudo handle remains valid; the writable struct
        // is exactly the byte size passed to the Windows API.
        assert_ne!(
            unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size) },
            0
        );
        eprintln!(
            "peak_working_set_mib={:.2}",
            counters.PeakWorkingSetSize as f64 / 1048576.0
        );
    }
}

#[test]
#[ignore = "writes and scans real large files; run explicitly with --release --ignored --nocapture"]
fn stress_large_file() {
    let gib = env_count("SNAPDIFF_STRESS_GIB", 1, 200);
    let root = tempfile::tempdir().unwrap();
    let bytes = gib * 1024 * 1024 * 1024;
    let path = root.path().join("large.bin");
    let started = Instant::now();
    let mut file = File::create(&path).unwrap();
    // Materialized data, not a sparse file: includes real write/read IO.
    let mut random = 0x1234_5678u32;
    let block: Vec<_> = (0..8 * 1024 * 1024)
        .map(|_| {
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            random as u8
        })
        .collect();
    let mut written = 0;
    while written < bytes {
        file.write_all(&block).unwrap();
        written += block.len() as u64;
        if written % (5 * 1024 * 1024 * 1024) == 0 {
            eprintln!("fixture_written_gib={}", written / (1024 * 1024 * 1024));
        }
    }
    file.sync_all().unwrap();
    drop(file);
    drop(block);
    eprintln!(
        "fixture_gib={gib} write_seconds={:.3}",
        started.elapsed().as_secs_f64()
    );
    let token = Arc::new(AtomicBool::new(false));
    let matcher = IgnoreMatcher::from_root(root.path(), &[]).unwrap();
    let start = Instant::now();
    let mut progress_events = 0;
    let entries = scanner::scan_directory(root.path(), &matcher, &token, |_| {
        progress_events += 1;
    })
    .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].size, bytes);
    assert_eq!(entries[0].md5.len(), 32);
    eprintln!(
        "scan_seconds={:.3} mib_per_second={:.1} progress_events={progress_events}",
        start.elapsed().as_secs_f64(),
        bytes as f64 / 1048576.0 / start.elapsed().as_secs_f64()
    );
    let mut cancel_requested = None;
    let result = scanner::scan_directory(root.path(), &matcher, &token, |p| {
        if cancel_requested.is_none() && p.current_file_bytes >= 1024 * 1024 {
            cancel_requested = Some(Instant::now());
            token.store(true, Ordering::Relaxed);
        }
    });
    assert_eq!(result.unwrap_err(), scanner::SCAN_CANCELLED);
    let latency = cancel_requested.unwrap().elapsed();
    eprintln!("cancel_latency_ms={:.3}", latency.as_secs_f64() * 1000.0);
    assert!(latency < Duration::from_secs(2));
    log_peak_memory();
    // TempDir removes only this generated fixture on success and unwinding.
}

#[test]
#[ignore = "creates thousands of real files; run explicitly with --release --ignored --nocapture"]
fn stress_many_files_and_history() {
    let count = env_count("SNAPDIFF_STRESS_FILES", 20_000, 200_000) as usize;
    let root = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let start = Instant::now();
    for i in 0..count {
        let folder = root.path().join(format!("group-{:04}", i / 100));
        if i % 100 == 0 {
            fs::create_dir(&folder).unwrap();
        }
        fs::write(
            folder.join(format!("file-{i:06}.txt")),
            format!("unique content {i}"),
        )
        .unwrap();
    }
    eprintln!(
        "small_files={count} fixture_seconds={:.3}",
        start.elapsed().as_secs_f64()
    );
    let start = Instant::now();
    let mut events = 0;
    let mut final_progress = ScanProgress::default();
    let files = scanner::scan_directory(
        root.path(),
        &IgnoreMatcher::from_root(root.path(), &[]).unwrap(),
        &Arc::new(AtomicBool::new(false)),
        |p| {
            events += 1;
            final_progress = p;
        },
    )
    .unwrap();
    eprintln!(
        "small_scan_seconds={:.3} progress_events={events}",
        start.elapsed().as_secs_f64()
    );
    assert_eq!(final_progress.processed, files.len());
    assert_eq!(final_progress.processed_bytes, final_progress.total_bytes);
    assert!(
        events <= (start.elapsed().as_millis() / 150) as usize + 8,
        "progress must be bounded by time, not file count"
    );
    let mut old = snapshot(root.path(), "old", files);
    assert_eq!(old.file_count, count);
    let mut new = old.clone();
    new.id = "new".into();
    for entry in new.files.iter_mut().filter(|e| e.kind == EntryKind::File) {
        entry.md5 = "a".repeat(32);
    }
    let start = Instant::now();
    let comparison = diff::compare(&old, &new, false, &[]).unwrap();
    assert_eq!(comparison.summary.counts.modified, count);
    eprintln!("compare_seconds={:.3}", start.elapsed().as_secs_f64());
    let query = DiffQuery {
        page: count / 200,
        sort: "time".into(),
        ..Default::default()
    };
    let start = Instant::now();
    let page = diff::query(&comparison, &query);
    assert_eq!(page.total, count);
    assert!(page.rows.len() <= 100);
    eprintln!("query_ms={:.3}", start.elapsed().as_secs_f64() * 1000.0);
    let mut store = storage::Store::open(data.path()).unwrap();
    let config = storage::new_config(root.path().to_str().unwrap(), &[]);
    old.directory_id = config.id.clone();
    new.directory_id = config.id.clone();
    let start = Instant::now();
    store.commit_scan(&config, &old, true, 0, true).unwrap();
    let config = store.config(&config.id).unwrap();
    store
        .commit_scan(&config, &new, true, count, false)
        .unwrap();
    eprintln!("two_commits_seconds={:.3}", start.elapsed().as_secs_f64());
    let start = Instant::now();
    assert_eq!(store.overviews().unwrap()[0].history_count, 2);
    eprintln!("list_ms={:.3}", start.elapsed().as_secs_f64() * 1000.0);
    drop(store);
    let store = storage::Store::open(data.path()).unwrap();
    let reopened = store.compare_states("old", "new", false, &[]).unwrap();
    assert_eq!(reopened.summary.counts.modified, count);
    eprintln!(
        "db_bytes={}",
        fs::metadata(data.path().join("snapdiff.sqlite3"))
            .unwrap()
            .len()
    );
    log_peak_memory();
}

#[path = "storage_benchmark.rs"]
mod storage_benchmark;
