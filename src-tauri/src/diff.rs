use crate::{ignore::IgnoreMatcher, models::*};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::atomic::{AtomicBool, Ordering},
};
use uuid::Uuid;

fn row(
    status: DiffStatus,
    old: Option<&FileEntry>,
    new: Option<&FileEntry>,
    focus: &IgnoreMatcher,
) -> DiffRow {
    let path = new.or(old).unwrap().relative_path.clone();
    let focused = [old, new]
        .into_iter()
        .flatten()
        .any(|e| focus.is_ignored(&e.relative_path, e.kind == EntryKind::Directory));
    DiffRow {
        path,
        status,
        old: old.cloned(),
        new: new.cloned(),
        focused,
        size_delta: new.map_or(0, |e| e.size as i128) - old.map_or(0, |e| e.size as i128),
    }
}

pub fn compare(
    old: &Snapshot,
    new: &Snapshot,
    current: bool,
    focus_patterns: &[String],
) -> Result<Comparison, String> {
    compare_cancellable(old, new, current, focus_patterns, None)
}

pub fn compare_cancellable(
    old: &Snapshot,
    new: &Snapshot,
    current: bool,
    focus_patterns: &[String],
    cancel: Option<&AtomicBool>,
) -> Result<Comparison, String> {
    let checkpoint = || {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            Err(crate::scanner::SCAN_CANCELLED.to_string())
        } else {
            Ok(())
        }
    };
    checkpoint()?;
    if old.directory_id != new.directory_id {
        return Err("只能比较同一个目录的状态".into());
    }
    let old_ignore = IgnoreMatcher::from_patterns(&old.effective_ignore_patterns)?;
    let new_ignore = IgnoreMatcher::from_patterns(&new.effective_ignore_patterns)?;
    let focus = IgnoreMatcher::from_patterns(focus_patterns)?;
    let directory_structure_available =
        old.directory_entries_complete && new.directory_entries_complete;
    // Compare the common observed scope. A changed exclusion is not a deletion.
    let comparable = |e: &&FileEntry| {
        !cancel.is_some_and(|c| c.load(Ordering::Relaxed))
            && (directory_structure_available || e.kind != EntryKind::Directory)
            && !old_ignore.is_ignored(&e.relative_path, e.kind == EntryKind::Directory)
            && !new_ignore.is_ignored(&e.relative_path, e.kind == EntryKind::Directory)
    };
    let old_map: BTreeMap<_, _> = old
        .files
        .iter()
        .filter(comparable)
        .map(|e| (&e.relative_path, e))
        .collect();
    let new_map: BTreeMap<_, _> = new
        .files
        .iter()
        .filter(comparable)
        .map(|e| (&e.relative_path, e))
        .collect();
    let excluded_scope_count = old
        .files
        .iter()
        .chain(new.files.iter())
        .filter(|e| {
            old_ignore.is_ignored(&e.relative_path, e.kind == EntryKind::Directory)
                || new_ignore.is_ignored(&e.relative_path, e.kind == EntryKind::Directory)
        })
        .map(|e| &e.relative_path)
        .collect::<HashSet<_>>()
        .len();
    checkpoint()?;
    let mut rows = Vec::new();
    let mut added = Vec::new();
    let mut deleted = Vec::new();
    for (path, entry) in &new_map {
        checkpoint()?;
        match old_map.get(path) {
            None => added.push(*entry),
            Some(previous) if previous.kind != entry.kind || previous.md5 != entry.md5 => rows
                .push(row(
                    DiffStatus::Modified,
                    Some(previous),
                    Some(entry),
                    &focus,
                )),
            Some(previous)
                if entry.kind != EntryKind::Directory
                    && (previous.size != entry.size
                        || previous.modified_time != entry.modified_time) =>
            {
                rows.push(row(
                    DiffStatus::Metadata,
                    Some(previous),
                    Some(entry),
                    &focus,
                ))
            }
            _ => {}
        }
    }
    for (path, entry) in &old_map {
        checkpoint()?;
        if !new_map.contains_key(path) {
            deleted.push(*entry);
        }
    }
    let mut added_by_hash: HashMap<(u64, &str), Vec<&FileEntry>> = HashMap::new();
    let mut deleted_by_hash: HashMap<(u64, &str), Vec<&FileEntry>> = HashMap::new();
    for entry in &added {
        checkpoint()?;
        if entry.kind == EntryKind::File {
            added_by_hash
                .entry((entry.size, &entry.md5))
                .or_default()
                .push(entry);
        }
    }
    for entry in &deleted {
        checkpoint()?;
        if entry.kind == EntryKind::File {
            deleted_by_hash
                .entry((entry.size, &entry.md5))
                .or_default()
                .push(entry);
        }
    }
    let mut moved_new = HashSet::new();
    let mut moved_old = HashSet::new();
    for (key, new_entries) in added_by_hash {
        checkpoint()?;
        if new_entries.len() != 1 {
            continue;
        }
        if let Some(old_entries) = deleted_by_hash.get(&key).filter(|items| items.len() == 1) {
            let previous = old_entries[0];
            let next = new_entries[0];
            moved_old.insert(previous.relative_path.clone());
            moved_new.insert(next.relative_path.clone());
            rows.push(row(DiffStatus::Moved, Some(previous), Some(next), &focus));
        }
    }
    for entry in added {
        checkpoint()?;
        if !moved_new.contains(&entry.relative_path) {
            rows.push(row(DiffStatus::Added, None, Some(entry), &focus));
        }
    }
    for entry in deleted {
        checkpoint()?;
        if !moved_old.contains(&entry.relative_path) {
            rows.push(row(DiffStatus::Deleted, Some(entry), None, &focus));
        }
    }
    rows.sort_by(|a, b| a.path.cmp(&b.path));
    let mut counts = DiffCounts::default();
    for entry in &rows {
        checkpoint()?;
        counts.add(entry.status);
    }
    let size_delta = new_map.values().map(|e| e.size as i128).sum::<i128>()
        - old_map.values().map(|e| e.size as i128).sum::<i128>();
    Ok(Comparison {
        summary: ComparisonSummary {
            id: Uuid::new_v4().to_string(),
            directory_id: old.directory_id.clone(),
            old: old.summary(),
            new: new.summary(),
            target_current: current,
            counts,
            size_delta,
            scope_changed: old.effective_ignore_patterns != new.effective_ignore_patterns,
            scope_unknown: old.schema_version == 1 || new.schema_version == 1,
            excluded_scope_count,
            directory_structure_available,
        },
        root_path: new.root_path.clone(),
        rows,
    })
}

fn query_rows<'a>(comparison: &'a Comparison, query: &DiffQuery) -> Vec<&'a DiffRow> {
    let search = query.search.trim().to_lowercase();
    let prefix = query.prefix.trim_matches('/');
    let mut rows: Vec<_> = comparison
        .rows
        .iter()
        .filter(|row| {
            (query.include_metadata || row.status != DiffStatus::Metadata)
                && query.status.is_none_or(|status| row.status == status)
                && (!query.focused_only || row.focused)
                && (prefix.is_empty() || row.path.starts_with(&format!("{prefix}/")))
                && (search.is_empty()
                    || row.path.to_lowercase().contains(&search)
                    || row
                        .old
                        .as_ref()
                        .is_some_and(|e| e.relative_path.to_lowercase().contains(&search)))
        })
        .collect();
    rows.sort_by(|a, b| {
        let order = match query.sort.as_str() {
            "size" => b.size_delta.abs().cmp(&a.size_delta.abs()),
            "time" => {
                let time = |r: &DiffRow| r.new.as_ref().or(r.old.as_ref()).unwrap().modified_time;
                time(b).cmp(&time(a))
            }
            "focus" => b.focused.cmp(&a.focused),
            _ => a.path.cmp(&b.path),
        };
        order.then_with(|| a.path.cmp(&b.path))
    });
    rows
}

pub fn query(comparison: &Comparison, query: &DiffQuery) -> DiffPage {
    const PAGE_SIZE: usize = 100;
    let matches = query_rows(comparison, query);
    let mut folders: BTreeMap<String, DiffFolder> = BTreeMap::new();
    let mut direct = Vec::new();
    if query.tree {
        let prefix = query.prefix.trim_matches('/');
        let prefix = if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix}/")
        };
        for row in &matches {
            let relative = row.path.strip_prefix(&prefix).unwrap_or(&row.path);
            if let Some((folder, _)) = relative.split_once('/') {
                let path = format!("{prefix}{folder}");
                let group = folders.entry(path.clone()).or_insert_with(|| DiffFolder {
                    path,
                    counts: DiffCounts::default(),
                    size_delta: 0,
                });
                group.counts.add(row.status);
                group.size_delta += row.size_delta;
            } else {
                direct.push(*row);
            }
        }
        direct.retain(|row| {
            if let Some(group) = folders.get_mut(&row.path) {
                if row.new.as_ref().or(row.old.as_ref()).unwrap().kind == EntryKind::Directory {
                    group.counts.add(row.status);
                    return false;
                }
            }
            true
        });
    } else {
        direct = matches;
    }
    let folders: Vec<_> = folders.into_values().collect();
    let total = folders.len() + direct.len();
    let page = query.page.min(total.saturating_sub(1) / PAGE_SIZE);
    let start = page * PAGE_SIZE;
    let end = (start + PAGE_SIZE).min(total);
    let visible_folders = folders
        .iter()
        .enumerate()
        .filter(|(i, _)| *i >= start && *i < end)
        .map(|(_, v)| v.clone())
        .collect();
    let visible_rows = direct
        .iter()
        .enumerate()
        .filter(|(i, _)| i + folders.len() >= start && i + folders.len() < end)
        .map(|(_, v)| (*v).clone())
        .collect();
    DiffPage {
        rows: visible_rows,
        folders: visible_folders,
        total,
        page,
        page_size: PAGE_SIZE,
    }
}

pub fn report(comparison: &Comparison, query: &DiffQuery) -> String {
    let s = &comparison.summary;
    let mut text = format!(
        "SnapDiff 目录差异报告\n目录: {}\n旧状态: {}\n新状态: {}{}\n新增 {} · 删除 {} · 修改 {} · 疑似移动 {} · 仅元数据 {}\n可比较范围大小变化: {:+} bytes\n",
        comparison.root_path, s.old.created_time, s.new.created_time,
        if s.target_current { "（当前扫描结果）" } else { "（历史快照）" },
        s.counts.added, s.counts.deleted, s.counts.modified, s.counts.moved, s.counts.metadata, s.size_delta
    );
    if s.scope_changed {
        text.push_str("注意: 忽略规则不同，仅比较两次均纳入的范围。\n");
    }
    if s.scope_unknown {
        text.push_str("注意: 旧版节点未保存完整忽略规则，新增/删除可能包含扫描范围变化。\n");
    }
    if !s.directory_structure_available {
        text.push_str("注意: 旧节点未记录完整目录结构，本次只比较文件和链接。\n");
    }
    text.push_str("\n以下条目按当前筛选条件导出（包含全部分页）：\n");
    for row in query_rows(comparison, query) {
        text.push_str(&format!("\n[{}] {}\n", row.status.label(), row.path));
        if let Some(old) = &row.old {
            text.push_str(&format!(
                "  旧: {} | {:?} | {} bytes | {} | MD5 {}\n",
                old.relative_path, old.kind, old.size, old.modified_time, old.md5
            ));
        }
        if let Some(new) = &row.new {
            text.push_str(&format!(
                "  新: {} | {:?} | {} bytes | {} | MD5 {}\n",
                new.relative_path, new.kind, new.size, new.modified_time, new.md5
            ));
        }
    }
    text
}

pub fn paths(comparison: &Comparison, query: &DiffQuery, absolute: bool) -> String {
    query_rows(comparison, query)
        .into_iter()
        .map(|row| {
            if absolute {
                std::path::Path::new(&comparison.root_path)
                    .join(&row.path)
                    .display()
                    .to_string()
            } else {
                row.path.clone()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
pub(crate) fn test_snapshot(id: &str, entries: Vec<FileEntry>) -> Snapshot {
    let mut snapshot = Snapshot {
        schema_version: 2,
        id: id.into(),
        directory_id: "folder".into(),
        name: "Folder".into(),
        created_time: chrono::Utc::now(),
        scan_started_time: None,
        root_path: "C:/folder".into(),
        ignore_patterns: vec![],
        effective_ignore_patterns: vec![],
        directory_entries_complete: true,
        remark: String::new(),
        file_count: 0,
        directory_count: 0,
        total_size: 0,
        type_stats: BTreeMap::new(),
        files: entries,
    };
    snapshot.recalculate_stats().unwrap();
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancelled_comparison_never_returns_partial_differences() {
        let old = test_snapshot("old", vec![]);
        let new = test_snapshot("new", vec![]);
        let token = AtomicBool::new(true);
        assert_eq!(
            compare_cancellable(&old, &new, true, &[], Some(&token)).unwrap_err(),
            crate::scanner::SCAN_CANCELLED
        );
    }
    fn file(path: &str, hash: &str) -> FileEntry {
        FileEntry {
            relative_path: path.into(),
            kind: EntryKind::File,
            size: 10,
            modified_time: chrono::Utc::now(),
            md5: hash.repeat(32),
        }
    }
    #[test]
    fn states_and_unique_move_detection() {
        let old = test_snapshot(
            "old",
            vec![
                file("change", "a"),
                file("old/path", "b"),
                file("delete", "c"),
            ],
        );
        let new = test_snapshot(
            "new",
            vec![file("change", "d"), file("new/path", "b"), file("add", "e")],
        );
        let diff = compare(&old, &new, false, &["new/".into()]).unwrap();
        assert_eq!(
            (
                diff.summary.counts.added,
                diff.summary.counts.deleted,
                diff.summary.counts.modified,
                diff.summary.counts.moved
            ),
            (1, 1, 1, 1)
        );
        assert!(
            diff.rows
                .iter()
                .find(|r| r.status == DiffStatus::Moved)
                .unwrap()
                .focused
        );
    }
    #[test]
    fn duplicate_content_does_not_claim_a_move() {
        let old = test_snapshot("old", vec![file("a", "a"), file("b", "a")]);
        let new = test_snapshot("new", vec![file("c", "a")]);
        let diff = compare(&old, &new, false, &[]).unwrap();
        assert_eq!(diff.summary.counts.moved, 0);
        assert_eq!(diff.summary.counts.deleted, 2);
    }
    #[test]
    fn metadata_empty_directories_and_changed_scope() {
        let old_file = file("same", "a");
        let mut new_file = old_file.clone();
        new_file.modified_time += chrono::Duration::seconds(1);
        let mut dir = file("empty", "");
        dir.kind = EntryKind::Directory;
        dir.size = 0;
        let old = test_snapshot("old", vec![old_file, file("cache/drop", "b")]);
        let mut new = test_snapshot("new", vec![new_file, dir]);
        new.effective_ignore_patterns = vec!["cache/".into()];
        let diff = compare(&old, &new, true, &[]).unwrap();
        assert_eq!(diff.summary.counts.metadata, 1);
        assert_eq!(diff.summary.counts.added, 1);
        assert_eq!(diff.summary.counts.deleted, 0);
        assert!(diff.summary.scope_changed);
        assert_eq!(query(&diff, &DiffQuery::default()).total, 1);
    }
    #[test]
    fn paging_tree_search_old_move_path_and_full_report() {
        let old = test_snapshot("old", vec![]);
        let new = test_snapshot(
            "new",
            (0..205)
                .map(|n| file(&format!("folder/{n:03}.bin"), "a"))
                .collect(),
        );
        let diff = compare(&old, &new, false, &[]).unwrap();
        assert_eq!(query(&diff, &DiffQuery::default()).rows.len(), 100);
        assert_eq!(
            query(
                &diff,
                &DiffQuery {
                    page: 2,
                    ..Default::default()
                }
            )
            .rows
            .len(),
            5
        );
        let tree = query(
            &diff,
            &DiffQuery {
                tree: true,
                ..Default::default()
            },
        );
        assert_eq!(tree.folders.len(), 1);
        assert_eq!(tree.folders[0].counts.added, 205);
        assert!(report(&diff, &DiffQuery::default()).contains("folder/204.bin"));
    }
}
