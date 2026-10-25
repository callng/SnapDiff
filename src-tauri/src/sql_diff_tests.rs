use super::*;
use crate::{diff, storage::Store};
use chrono::{TimeZone, Utc};

fn file(path: &str, key: u32) -> FileEntry {
    FileEntry {
        relative_path: path.into(),
        kind: EntryKind::File,
        size: key as u64 + 10,
        modified_time: Utc
            .timestamp_opt(1_700_000_000 + key as i64, 123_456_789)
            .unwrap(),
        md5: format!("{key:032x}"),
    }
}
fn directory(path: &str) -> FileEntry {
    let mut e = file(path, 0);
    e.kind = EntryKind::Directory;
    e.size = 0;
    e.md5.clear();
    e
}
fn snapshots() -> (Snapshot, Snapshot) {
    let mut entries = vec![
        file("metadata", 1),
        file("modified", 2),
        file("type", 3),
        file("deleted", 4),
        file("old/移动😀.dll", 5),
        file("duplicate/a", 6),
        file("duplicate/b", 6),
        file("ignored/drop", 7),
        directory("folder"),
        directory("empty"),
        file("unchanged", 8),
    ];
    for i in 0..215 {
        entries.push(file(&format!("中文/组/{i:03}.bin"), i + 100));
    }
    let old = diff::test_snapshot("old", entries);
    let mut new = old.clone();
    new.id = "new".into();
    new.files.retain(|e| {
        !matches!(
            e.relative_path.as_str(),
            "deleted" | "old/移动😀.dll" | "duplicate/a" | "duplicate/b" | "empty"
        )
    });
    for e in &mut new.files {
        if e.relative_path == "metadata" {
            e.modified_time += chrono::Duration::nanoseconds(1);
        }
        if e.relative_path == "modified" {
            e.md5 = format!("{:032x}", 999);
            e.size += 17;
        }
        if e.relative_path == "type" {
            e.kind = EntryKind::Symlink;
        }
        if e.relative_path.starts_with("中文/") {
            e.md5 = format!("{:032x}", 999);
        }
        if e.relative_path == "folder" {
            e.modified_time += chrono::Duration::hours(1);
        }
    }
    new.files.extend([
        file("new/移动😀.dll", 5),
        file("duplicate/c", 6),
        file("folder/A%_ß.dll", 9),
        file("added", 10),
        directory("new-empty"),
    ]);
    new.recalculate_stats().unwrap();
    (old, new)
}
fn persisted(old: &mut Snapshot, new: &mut Snapshot) -> (tempfile::TempDir, Store) {
    let temp = tempfile::tempdir().unwrap();
    let mut store = Store::open(temp.path()).unwrap();
    let config = storage::new_config("C:/folder", &[]);
    old.directory_id = config.id.clone();
    new.directory_id = config.id.clone();
    store.commit_scan(&config, old, true, 0, true).unwrap();
    let config = store.config(&config.id).unwrap();
    store.commit_scan(&config, new, true, 0, false).unwrap();
    (temp, store)
}
fn assert_queries(actual: &SqlComparison, reference: &Comparison) {
    let mut queries = Vec::new();
    for tree in [false, true] {
        for prefix in ["", "中文", "中文/组", "folder", "new", "missing"] {
            for sort in ["", "size", "time", "focus"] {
                for page in [0, 1, 2, 999, usize::MAX] {
                    queries.push(DiffQuery {
                        tree,
                        prefix: prefix.into(),
                        sort: sort.into(),
                        page,
                        include_metadata: true,
                        ..Default::default()
                    });
                }
            }
        }
    }
    for search in ["移动", "OLD/", "A%_ß", "中文", "notfound"] {
        queries.push(DiffQuery {
            search: search.into(),
            ..Default::default()
        });
    }
    for status in [
        DiffStatus::Added,
        DiffStatus::Deleted,
        DiffStatus::Modified,
        DiffStatus::Moved,
        DiffStatus::Metadata,
    ] {
        queries.push(DiffQuery {
            status: Some(status),
            include_metadata: true,
            ..Default::default()
        });
        queries.push(DiffQuery {
            status: Some(status),
            ..Default::default()
        });
    }
    queries.extend([
        DiffQuery::default(),
        DiffQuery {
            focused_only: true,
            ..Default::default()
        },
    ]);
    for query in queries {
        let expected = diff::query(reference, &query);
        assert_eq!(
            serde_json::to_value(actual.query(&query).unwrap()).unwrap(),
            serde_json::to_value(expected).unwrap(),
            "{query:?}"
        );
        assert_eq!(
            actual.paths(&query, false).unwrap(),
            diff::paths(reference, &query, false),
            "{query:?}"
        );
    }
    assert_eq!(actual.summary.size_delta, reference.summary.size_delta);
    assert_eq!(
        actual.summary.excluded_scope_count,
        reference.summary.excluded_scope_count
    );
    assert_eq!(
        serde_json::to_value(&actual.summary.counts).unwrap(),
        serde_json::to_value(&reference.summary.counts).unwrap()
    );
}

#[test]
fn sql_diff_matches_reference_filter_sort_tree_search_and_paging() {
    let (mut old, mut new) = snapshots();
    let (_temp, mut store) = persisted(&mut old, &mut new);
    let focus = vec!["new/".into(), "中文/组/00*".into()];
    let reference = diff::compare(&old, &new, false, &focus).unwrap();
    let actual = store.compare_states("old", "new", false, &focus).unwrap();
    assert_queries(&actual, &reference);
    let query = DiffQuery {
        include_metadata: true,
        search: "移动".into(),
        ..Default::default()
    };
    let report = diff::report(&reference, &query);
    assert_eq!(actual.report_preview(&query).unwrap()["text"], report);
    let out = tempfile::tempdir().unwrap();
    let path = out.path().join("report.txt");
    actual.export_report(&query, &path).unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), report);
    actual.set_focus(&["old/".into()]).unwrap();
    let focused = actual
        .query(&DiffQuery {
            focused_only: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(focused.total, 1);
    assert_eq!(focused.rows[0].status, DiffStatus::Moved);
    let tree = DiffQuery {
        tree: true,
        focused_only: true,
        ..Default::default()
    };
    assert_eq!(actual.query(&tree).unwrap().folders[0].path, "new");
    actual.set_focus(&["folder/".into()]).unwrap();
    assert_eq!(actual.query(&tree).unwrap().folders[0].path, "folder");
    let archive_path = out.path().join("archive.json");
    store
        .export_archive(
            Some(&old.directory_id),
            &["old".into(), "new".into()],
            &archive_path,
        )
        .unwrap();
    let loaded = storage::read_archive(&archive_path).unwrap();
    let expected = store.archive(Some(&old.directory_id), &[]).unwrap();
    assert_eq!(
        serde_json::to_value(&loaded.snapshots).unwrap(),
        serde_json::to_value(&expected.snapshots).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&loaded.directories).unwrap(),
        serde_json::to_value(&expected.directories).unwrap()
    );
    let before = std::fs::read(&archive_path).unwrap();
    assert!(store
        .export_archive(None, &["missing".into()], &archive_path)
        .is_err());
    assert_eq!(std::fs::read(&archive_path).unwrap(), before);
}

#[test]
fn changed_scope_legacy_structure_and_staged_scan_match_reference() {
    for legacy in [false, true] {
        let (mut old, mut new) = snapshots();
        old.effective_ignore_patterns = vec!["ignored/".into(), "!ignored/keep".into()];
        new.effective_ignore_patterns = vec!["duplicate/".into(), "/type/".into()];
        if legacy {
            old.schema_version = 1;
            old.directory_entries_complete = false;
        }
        let (_temp, mut store) = persisted(&mut old, &mut new);
        let reference = diff::compare(&old, &new, true, &[]).unwrap();
        let actual = store
            .compare_scan("old", &new, true, &[], Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert_queries(&actual, &reference);
        assert_eq!(actual.summary.scope_unknown, legacy);
        assert_eq!(actual.summary.directory_structure_available, !legacy);
    }
}

#[test]
fn comparison_survives_retention_and_report_preview_is_bounded() {
    let (mut old, mut new) = snapshots();
    new.files
        .extend((0..500).map(|i| file(&format!("reports/{i}"), i + 1000)));
    new.recalculate_stats().unwrap();
    let (_temp, mut store) = persisted(&mut old, &mut new);
    let reference = diff::compare(&old, &new, false, &[]).unwrap();
    let comparison = store.compare_states("old", "new", false, &[]).unwrap();
    store
        .update_settings(&old.directory_id, vec![], vec![], 1)
        .unwrap();
    assert!(store.snapshot("old").is_err());
    let query = DiffQuery::default();
    assert_eq!(
        comparison.query(&query).unwrap().total,
        reference
            .rows
            .iter()
            .filter(|r| r.status != DiffStatus::Metadata)
            .count()
    );
    let preview = comparison.report_preview(&query).unwrap();
    let report = diff::report(&reference, &query);
    assert_eq!(preview["bytes"], report.len());
    assert_eq!(preview["truncated"], true);
    let mut end = report.len().min(64 * 1024);
    while !report.is_char_boundary(end) {
        end -= 1;
    }
    assert_eq!(
        preview["text"],
        format!(
            "{}\n\n（预览已截断；保存文本文件可导出完整报告。）",
            &report[..end]
        )
    );
    assert!(preview["text"].as_str().unwrap().len() < 66_000);
    let out = tempfile::tempdir().unwrap();
    let path = out.path().join("report.txt");
    comparison.export_report(&query, &path).unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), report);
}

#[test]
fn many_unique_moves_do_not_skip_rows_and_wide_tree_sum_does_not_overflow() {
    let mut old = diff::test_snapshot(
        "old",
        (0..150).map(|i| file(&format!("old/{i}"), i)).collect(),
    );
    let mut new = diff::test_snapshot(
        "new",
        (0..150).map(|i| file(&format!("new/{i}"), i)).collect(),
    );
    // A sum can exceed i64 even when every single entry fits SQLite INTEGER.
    let mut huge = file("huge/one", 1000);
    huge.size = i64::MAX as u64;
    new.files.extend([huge, file("huge/two", 1001)]);
    new.recalculate_stats().unwrap();
    let (_temp, mut store) = persisted(&mut old, &mut new);
    let actual = store.compare_states("old", "new", false, &[]).unwrap();
    let reference = diff::compare(&old, &new, false, &[]).unwrap();
    assert_queries(&actual, &reference);
    assert_eq!(actual.summary.counts.moved, 150);
}

#[test]
fn cancellation_interrupts_sql_and_cleans_staging_without_mutating_history() {
    let mut old = diff::test_snapshot(
        "old",
        (0..20_000)
            .map(|i| file(&format!("file/{i:06}"), i))
            .collect(),
    );
    let mut new = old.clone();
    new.id = "new".into();
    let (_temp, mut store) = persisted(&mut old, &mut new);
    let token = Arc::new(AtomicBool::new(false));
    let signal = token.clone();
    let worker = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(5));
        signal.store(true, Ordering::Relaxed);
    });
    let result = store.compare_scan("old", &new, true, &[], token);
    worker.join().unwrap();
    assert_eq!(result.err().unwrap(), SCAN_CANCELLED);
    assert_eq!(store.history(&old.directory_id).unwrap().len(), 2);
    // The cancellation hook and staging must not poison subsequent operations.
    let actual = store
        .compare_scan("old", &new, true, &[], Arc::new(AtomicBool::new(false)))
        .unwrap();
    assert_eq!(actual.row_count, 0);
}
