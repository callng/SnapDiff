//! Synthetic metadata only: no user data and no large file IO required.
use super::*;
use rusqlite::{params, Connection};

fn legacy_insert(conn: &mut Connection, node: &Snapshot) {
    let tx = conn.transaction().unwrap();
    let mut insert = tx
        .prepare("INSERT INTO entries VALUES(?1,?2,?3,?4,?5,?6)")
        .unwrap();
    for e in &node.files {
        insert
            .execute(params![
                node.id,
                e.relative_path,
                crate::entry_codec::kind_name(e.kind),
                e.size as i64,
                e.modified_time.to_rfc3339(),
                e.md5
            ])
            .unwrap();
    }
    drop(insert);
    tx.commit().unwrap();
}
fn legacy_read(conn: &Connection, header: &Snapshot) -> Snapshot {
    let mut result = header.header();
    let mut stmt = conn
        .prepare(
            "SELECT path,kind,size,modified_time,md5 FROM entries WHERE state_id=?1 ORDER BY path",
        )
        .unwrap();
    let mut rows = stmt.query([&header.id]).unwrap();
    while let Some(row) = rows.next().unwrap() {
        result.files.push(FileEntry {
            relative_path: row.get(0).unwrap(),
            kind: EntryKind::File,
            size: row.get(2).unwrap(),
            modified_time: chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>(3).unwrap())
                .unwrap()
                .with_timezone(&chrono::Utc),
            md5: row.get(4).unwrap(),
        });
    }
    result
}
#[test]
#[ignore = "synthetic storage/query benchmark; run --release --ignored --nocapture"]
fn stress_storage_queries() {
    let count = env_count("SNAPDIFF_STORAGE_FILES", 100_000, 1_000_000) as usize;
    for percent in [1, 100] {
        let data = tempfile::tempdir().unwrap();
        let legacy_data = tempfile::tempdir().unwrap();
        let legacy_path = legacy_data.path().join("legacy.sqlite3");
        let mut legacy = Connection::open(&legacy_path).unwrap();
        legacy.execute_batch("PRAGMA journal_mode=WAL;
            CREATE TABLE entries(state_id TEXT NOT NULL,path TEXT NOT NULL,kind TEXT NOT NULL,size INTEGER NOT NULL,modified_time TEXT NOT NULL,md5 TEXT NOT NULL,PRIMARY KEY(state_id,path));").unwrap();
        let config = storage::new_config("C:/synthetic-storage-test", &[]);
        let time = chrono::DateTime::parse_from_rfc3339("2026-10-12T00:00:00.123456789Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let mut old = diff::test_snapshot(
            &uuid::Uuid::new_v4().to_string(),
            (0..count)
                .map(|i| FileEntry {
                    relative_path: format!("group-{:04}/file-{i:06}.bin", i / 100),
                    kind: EntryKind::File,
                    size: (i + 10) as u64,
                    modified_time: time + chrono::Duration::seconds(i as i64),
                    md5: format!("{i:032x}"),
                })
                .collect(),
        );
        old.directory_id = config.id.clone();
        old.root_path = config.root_path.clone();
        let mut new = old.clone();
        new.id = uuid::Uuid::new_v4().to_string();
        let changed = count * percent / 100;
        for e in new.files.iter_mut().take(changed) {
            e.md5 = "f".repeat(32);
        }
        let start = Instant::now();
        legacy_insert(&mut legacy, &old);
        legacy_insert(&mut legacy, &new);
        let legacy_write = start.elapsed();
        let mut store = storage::Store::open(data.path()).unwrap();
        let start = Instant::now();
        store.commit_scan(&config, &old, true, 0, true).unwrap();
        store
            .commit_scan(
                &store.config(&config.id).unwrap(),
                &new,
                true,
                changed,
                false,
            )
            .unwrap();
        let compact_write = start.elapsed();
        legacy
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        let compact_conn = Connection::open(data.path().join("snapdiff.sqlite3")).unwrap();
        compact_conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        drop(compact_conn);
        let start = Instant::now();
        let reference = diff::compare(
            &legacy_read(&legacy, &old),
            &legacy_read(&legacy, &new),
            false,
            &[],
        )
        .unwrap();
        let legacy_compare = start.elapsed();
        let query = DiffQuery {
            sort: "time".into(),
            page: changed / 200,
            ..Default::default()
        };
        let start = Instant::now();
        let expected = diff::query(&reference, &query);
        let legacy_query = start.elapsed();
        let start = Instant::now();
        let actual = store.compare_states(&old.id, &new.id, false, &[]).unwrap();
        let sql_compare = start.elapsed();
        let start = Instant::now();
        let page = actual.query(&query).unwrap();
        let sql_query = start.elapsed();
        assert_eq!(actual.summary.counts.modified, changed);
        assert_eq!(
            serde_json::to_value(page).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        let tree = DiffQuery {
            tree: true,
            ..Default::default()
        };
        let start = Instant::now();
        let tree_page = actual.query(&tree).unwrap();
        let tree_ms = start.elapsed().as_secs_f64() * 1000.;
        let start = Instant::now();
        actual
            .query(&DiffQuery {
                tree: true,
                page: 5,
                ..Default::default()
            })
            .unwrap();
        let tree_cached_ms = start.elapsed().as_secs_f64() * 1000.;
        assert_eq!(
            serde_json::to_value(tree_page).unwrap(),
            serde_json::to_value(diff::query(&reference, &tree)).unwrap()
        );
        eprintln!("files={count} changed_percent={percent} legacy_write_ms={:.2} compact_write_ms={:.2} legacy_db_bytes={} compact_db_bytes={} legacy_load_compare_ms={:.2} sql_build_ms={:.2} legacy_page_ms={:.3} sql_page_ms={:.3} sql_tree_ms={tree_ms:.3} sql_tree_cached_ms={tree_cached_ms:.3}",
            legacy_write.as_secs_f64()*1000.,compact_write.as_secs_f64()*1000.,fs::metadata(&legacy_path).unwrap().len(),fs::metadata(data.path().join("snapdiff.sqlite3")).unwrap().len(),
            legacy_compare.as_secs_f64()*1000.,sql_compare.as_secs_f64()*1000.,legacy_query.as_secs_f64()*1000.,sql_query.as_secs_f64()*1000.);
        // Include a deep but genuinely empty history to measure header loading.
        let history = if percent == 1 { 10_000 } else { 0 };
        let start = Instant::now();
        for i in 0..history {
            let mut empty = diff::test_snapshot(&format!("history-{i}"), vec![]);
            empty.directory_id = config.id.clone();
            store
                .commit_scan(&store.config(&config.id).unwrap(), &empty, true, 0, false)
                .unwrap();
        }
        eprintln!(
            "history_fixture_ms={:.2}",
            start.elapsed().as_secs_f64() * 1000.
        );
        let start = Instant::now();
        let nodes = store.history(&config.id).unwrap();
        let header_ms = start.elapsed().as_secs_f64() * 1000.;
        let start = Instant::now();
        let overview = store.overview(&config.id).unwrap();
        let overview_ms = start.elapsed().as_secs_f64() * 1000.;
        assert_eq!(overview.history_count, history + 2);
        assert_eq!(overview.latest.unwrap().id, nodes[0].id);
        eprintln!(
            "history_nodes={} full_history_ms={header_ms:.3} overview_ms={overview_ms:.3}",
            history + 2
        );
    }
}
