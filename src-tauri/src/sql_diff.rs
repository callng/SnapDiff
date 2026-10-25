//! Disk-backed, disposable comparisons. Only changed entries and the requested
//! page cross into Rust; unchanged files never become FileEntry allocations.
use crate::{entry_codec, ignore::IgnoreMatcher, models::*, scanner::SCAN_CANCELLED, storage};
use rusqlite::{
    functions::{Aggregate, Context, FunctionFlags},
    params, Connection, OptionalExtension,
};
use std::{
    io::Write,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use uuid::Uuid;

#[cfg(test)]
#[path = "sql_diff_tests.rs"]
mod tests;

fn sql<T>(result: rusqlite::Result<T>) -> Result<T, String> {
    result.map_err(|e| match e {
        rusqlite::Error::SqliteFailure(ref error, _)
            if error.code == rusqlite::ErrorCode::OperationInterrupted =>
        {
            SCAN_CANCELLED.into()
        }
        _ => format!("差异数据库错误: {e}"),
    })
}
fn checkpoint(cancel: Option<&Arc<AtomicBool>>) -> Result<(), String> {
    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
        Err(SCAN_CANCELLED.into())
    } else {
        Ok(())
    }
}
fn status_number(status: DiffStatus) -> i64 {
    match status {
        DiffStatus::Added => 0,
        DiffStatus::Deleted => 1,
        DiffStatus::Modified => 2,
        DiffStatus::Moved => 3,
        DiffStatus::Metadata => 4,
    }
}
fn status_value(status: i64) -> DiffStatus {
    match status {
        0 => DiffStatus::Added,
        1 => DiffStatus::Deleted,
        2 => DiffStatus::Modified,
        3 => DiffStatus::Moved,
        _ => DiffStatus::Metadata,
    }
}
fn make_row(
    status: DiffStatus,
    old: Option<FileEntry>,
    new: Option<FileEntry>,
    focus: &IgnoreMatcher,
) -> DiffRow {
    let path = new.as_ref().or(old.as_ref()).unwrap().relative_path.clone();
    let focused = [old.as_ref(), new.as_ref()]
        .into_iter()
        .flatten()
        .any(|e| focus.is_ignored(&e.relative_path, e.kind == EntryKind::Directory));
    let size_delta =
        new.as_ref().map_or(0, |e| e.size as i128) - old.as_ref().map_or(0, |e| e.size as i128);
    DiffRow {
        path,
        status,
        old,
        new,
        size_delta,
        focused,
    }
}
fn payload(row: &DiffRow) -> Result<String, String> {
    serde_json::to_string(row).map_err(|e| e.to_string())
}
fn decode(text: String) -> Result<DiffRow, String> {
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

// SQL SUM would overflow for valid aggregates larger than i64; keep i128
// semantics used by the public model, including directory-tree grouping.
struct Sum128;
impl Aggregate<i128, String> for Sum128 {
    fn init(&self, _: &mut Context<'_>) -> rusqlite::Result<i128> {
        Ok(0)
    }
    fn step(&self, ctx: &mut Context<'_>, sum: &mut i128) -> rusqlite::Result<()> {
        *sum += ctx.get::<i64>(0)? as i128;
        Ok(())
    }
    fn finalize(&self, _: &mut Context<'_>, sum: Option<i128>) -> rusqlite::Result<String> {
        Ok(sum.unwrap_or(0).to_string())
    }
}

// Always remove staging rows, callbacks and cancellation hooks, even on error.
struct SourceGuard<'a>(&'a mut Connection);
impl Drop for SourceGuard<'_> {
    fn drop(&mut self) {
        let _ = self.0.progress_handler(0, None::<fn() -> bool>);
        let _ = self
            .0
            .execute_batch("ROLLBACK TO scan_stage; RELEASE scan_stage;");
        let _ = self.0.remove_function("sd_excluded", 2);
        let _ = self
            .0
            .execute_batch("DROP TABLE IF EXISTS temp.scan_entries");
    }
}

pub struct SqlComparison {
    pub summary: ComparisonSummary,
    pub row_count: usize,
    root_path: String,
    conn: Mutex<Connection>,
}

impl SqlComparison {
    pub fn build(
        source: &mut Connection,
        old: &Snapshot,
        new: &Snapshot,
        current: bool,
        focus_patterns: &[String],
        cancel: Option<Arc<AtomicBool>>,
        staged: bool,
    ) -> Result<Self, String> {
        checkpoint(cancel.as_ref())?;
        if old.directory_id != new.directory_id {
            return Err("只能比较同一个目录的状态".into());
        }
        let old_ignore = Arc::new(IgnoreMatcher::from_patterns(
            &old.effective_ignore_patterns,
        )?);
        let new_ignore = Arc::new(IgnoreMatcher::from_patterns(
            &new.effective_ignore_patterns,
        )?);
        let focus = IgnoreMatcher::from_patterns(focus_patterns)?;
        let structure = old.directory_entries_complete && new.directory_entries_complete;
        let guard = SourceGuard(source);
        if let Some(token) = cancel.clone() {
            sql(guard
                .0
                .progress_handler(1000, Some(move || token.load(Ordering::Relaxed))))?;
        }
        let source = &*guard.0;
        let old_rules = old_ignore.clone();
        let new_rules = new_ignore.clone();
        sql(source.create_scalar_function(
            "sd_excluded",
            2,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            move |ctx| {
                let path = ctx.get::<String>(0)?;
                let directory = ctx.get::<String>(1)? == "directory";
                Ok(
                    old_rules.is_ignored(&path, directory)
                        || new_rules.is_ignored(&path, directory),
                )
            },
        ))?;
        let comparable = |entry: &FileEntry| {
            (structure || entry.kind != EntryKind::Directory)
                && !old_ignore.is_ignored(&entry.relative_path, entry.kind == EntryKind::Directory)
                && !new_ignore.is_ignored(&entry.relative_path, entry.kind == EntryKind::Directory)
        };
        let new_table = if staged {
            sql(source.execute_batch("DROP TABLE IF EXISTS temp.scan_entries;
                CREATE TEMP TABLE scan_entries(state_id TEXT NOT NULL,path TEXT NOT NULL,kind TEXT NOT NULL,size INTEGER NOT NULL,
                modified_seconds INTEGER NOT NULL,modified_nanos INTEGER NOT NULL,md5 BLOB NOT NULL,PRIMARY KEY(state_id,path)) WITHOUT ROWID;"))?;
            sql(source.execute_batch("SAVEPOINT scan_stage"))?;
            for chunk in new.files.chunks(512) {
                checkpoint(cancel.as_ref())?;
                let result =
                    entry_codec::insert_entries(source, "temp.scan_entries", &new.id, chunk);
                checkpoint(cancel.as_ref())?;
                result?;
            }
            sql(source.execute_batch("RELEASE scan_stage"))?;
            "temp.scan_entries"
        } else {
            "entries"
        };
        // Empty SQLite filename creates an automatically deleted disk-backed DB.
        // Unsafe durability settings apply only to this disposable comparison.
        let mut conn = sql(Connection::open(""))?;
        sql(conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF;
            PRAGMA cache_size=-2048; PRAGMA temp_store=FILE;
            CREATE TABLE changes(id INTEGER PRIMARY KEY,path TEXT NOT NULL UNIQUE,old_path TEXT NOT NULL,old_kind TEXT NOT NULL,
                search_path TEXT NOT NULL,search_old TEXT NOT NULL,status INTEGER NOT NULL,
                focused INTEGER NOT NULL,delta INTEGER NOT NULL,abs_delta INTEGER NOT NULL,
                seconds INTEGER NOT NULL,nanos INTEGER NOT NULL,kind TEXT NOT NULL,
                size INTEGER NOT NULL,md5 BLOB NOT NULL);
            CREATE TABLE details(id INTEGER PRIMARY KEY REFERENCES changes(id) ON DELETE CASCADE,payload TEXT NOT NULL);
            CREATE TEMP TABLE tree_key(value TEXT);
            BEGIN;"))?;
        if let Some(token) = cancel.clone() {
            sql(conn.progress_handler(1000, Some(move || token.load(Ordering::Relaxed))))?;
        }
        // EXCEPT performs sequential scans with disk-backed set operations.
        // Point-joining every path regresses badly on wide WITHOUT ROWID keys.
        // Merge just the two ordered candidate streams with bounded Rust memory.
        let old_query=format!("SELECT {0} FROM entries WHERE state_id=?1 EXCEPT SELECT {0} FROM {new_table} WHERE state_id=?2 ORDER BY path",entry_codec::COLUMNS);
        let new_query=format!("SELECT {0} FROM {new_table} WHERE state_id=?2 EXCEPT SELECT {0} FROM entries WHERE state_id=?1 ORDER BY path",entry_codec::COLUMNS);
        let mut old_stmt = sql(source.prepare(&old_query))?;
        let mut new_stmt = sql(source.prepare(&new_query))?;
        let mut old_reader = sql(old_stmt.query(params![old.id, new.id]))?;
        let mut new_reader = sql(new_stmt.query(params![old.id, new.id]))?;
        let next = |reader: &mut rusqlite::Rows<'_>| -> Result<Option<FileEntry>, String> {
            while let Some(record) = sql(reader.next())? {
                checkpoint(cancel.as_ref())?;
                let entry = sql(entry_codec::read_entry(record, 0))?;
                if comparable(&entry) {
                    return Ok(Some(entry));
                }
            }
            Ok(None)
        };
        let mut previous = next(&mut old_reader)?;
        let mut following = next(&mut new_reader)?;
        let mut size_delta = 0i128;
        while previous.is_some() || following.is_some() {
            checkpoint(cancel.as_ref())?;
            let order = match (&previous, &following) {
                (Some(o), Some(n)) => o.relative_path.cmp(&n.relative_path),
                (Some(_), None) => std::cmp::Ordering::Less,
                _ => std::cmp::Ordering::Greater,
            };
            let (old_entry, new_entry) = match order {
                std::cmp::Ordering::Less => {
                    let entry = previous.take();
                    previous = next(&mut old_reader)?;
                    (entry, None)
                }
                std::cmp::Ordering::Greater => {
                    let entry = following.take();
                    following = next(&mut new_reader)?;
                    (None, entry)
                }
                std::cmp::Ordering::Equal => {
                    let entries = (previous.take(), following.take());
                    previous = next(&mut old_reader)?;
                    following = next(&mut new_reader)?;
                    entries
                }
            };
            let status = match (&old_entry, &new_entry) {
                (None, _) => DiffStatus::Added,
                (_, None) => DiffStatus::Deleted,
                (Some(o), Some(n)) if o.kind != n.kind || o.md5 != n.md5 => DiffStatus::Modified,
                (_, Some(n)) if n.kind == EntryKind::Directory => continue,
                _ => DiffStatus::Metadata,
            };
            let row = make_row(status, old_entry, new_entry, &focus);
            size_delta += row.size_delta;
            Self::insert(&conn, &row)?;
        }
        drop(old_reader);
        drop(new_reader);
        drop(old_stmt);
        drop(new_stmt);
        checkpoint(cancel.as_ref())?;
        let excluded_scope_count = if old.effective_ignore_patterns.is_empty()
            && new.effective_ignore_patterns.is_empty()
        {
            0
        } else {
            sql(source.query_row(&format!(
            "SELECT COUNT(*) FROM (SELECT path FROM entries WHERE state_id=?1 AND sd_excluded(path,kind)
             UNION SELECT path FROM {new_table} WHERE state_id=?2 AND sd_excluded(path,kind))"
        ), params![old.id, new.id], |r| r.get(0)))?
        };

        sql(conn.execute_batch("CREATE INDEX move_hash ON changes(status,kind,size,md5);
            CREATE TEMP TABLE moves AS
            SELECT MIN(CASE WHEN status=0 THEN id END) added,MIN(CASE WHEN status=1 THEN id END) deleted
            FROM changes WHERE status IN (0,1) AND kind='file' GROUP BY size,md5
            HAVING SUM(status=0)=1 AND SUM(status=1)=1;"))?;
        // Scan the immutable pairing table. Mutating a table traversed by a
        // live SELECT can otherwise revisit/skip records in SQLite.
        let mut move_stmt = sql(conn.prepare("SELECT added,deleted FROM moves"))?;
        let mut moves = sql(move_stmt.query([]))?;
        while let Some(record) = sql(moves.next())? {
            checkpoint(cancel.as_ref())?;
            let added_id: i64 = sql(record.get(0))?;
            let deleted_id: i64 = sql(record.get(1))?;
            let added = decode(sql(conn.query_row(
                "SELECT payload FROM details WHERE id=?1",
                [added_id],
                |r| r.get(0),
            ))?)?;
            let deleted = decode(sql(conn.query_row(
                "SELECT payload FROM details WHERE id=?1",
                [deleted_id],
                |r| r.get(0),
            ))?)?;
            let row = make_row(DiffStatus::Moved, deleted.old, added.new, &focus);
            sql(conn.execute(
                "DELETE FROM changes WHERE id IN (?1,?2)",
                params![added_id, deleted_id],
            ))?;
            Self::insert(&conn, &row)?;
        }
        drop(moves);
        drop(move_stmt);

        sql(conn.execute_batch(
            "DROP TABLE moves; DROP INDEX move_hash;
            CREATE INDEX changes_size ON changes(abs_delta DESC,path,status,focused);
            CREATE INDEX changes_time ON changes(seconds DESC,nanos DESC,path,status,focused);
            CREATE INDEX changes_focus ON changes(focused DESC,path,status);
            CREATE INDEX changes_status ON changes(status);
            CREATE INDEX changes_path ON changes(path,status,focused);
            COMMIT;",
        ))?;
        let mut counts = DiffCounts::default();
        let mut stmt = sql(conn.prepare("SELECT status,COUNT(*) FROM changes GROUP BY status"))?;
        let mut rows = sql(stmt.query([]))?;
        while let Some(row) = sql(rows.next())? {
            let count: usize = sql(row.get(1))?;
            match status_value(sql(row.get(0))?) {
                DiffStatus::Added => counts.added = count,
                DiffStatus::Deleted => counts.deleted = count,
                DiffStatus::Modified => counts.modified = count,
                DiffStatus::Moved => counts.moved = count,
                DiffStatus::Metadata => counts.metadata = count,
            }
        }
        drop(rows);
        drop(stmt);
        checkpoint(cancel.as_ref())?;
        sql(conn.progress_handler(0, None::<fn() -> bool>))?;
        sql(conn.create_aggregate_function(
            "sd_sum",
            1,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            Sum128,
        ))?;
        Ok(Self {
            row_count: counts.content_total() + counts.metadata,
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
                directory_structure_available: structure,
            },
            root_path: new.root_path.clone(),
            conn: Mutex::new(conn),
        })
    }

    fn insert(conn: &Connection, row: &DiffRow) -> Result<(), String> {
        let entry = row.new.as_ref().or(row.old.as_ref()).unwrap();
        let old_path = row.old.as_ref().map_or("", |e| e.relative_path.as_str());
        let delta = i64::try_from(row.size_delta).map_err(|_| "差异大小超出范围")?;
        sql(conn.prepare_cached("INSERT INTO changes(path,old_path,old_kind,search_path,search_old,status,focused,delta,abs_delta,seconds,nanos,kind,size,md5)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)"))?.execute(params![
                row.path, old_path, row.old.as_ref().map_or("file",|e|entry_codec::kind_name(e.kind)), row.path.to_lowercase(), old_path.to_lowercase(), status_number(row.status), row.focused,
                delta, delta.abs(), entry.modified_time.timestamp(), entry.modified_time.timestamp_subsec_nanos(),
                entry_codec::kind_name(entry.kind), i64::try_from(entry.size).map_err(|_| "文件大小超出范围")?,
                entry_codec::hash_bytes(&entry.md5)?
            ]).map_err(|e| e.to_string())?;
        sql(conn.prepare_cached("INSERT INTO details(id,payload) VALUES(?1,?2)"))?
            .execute(params![conn.last_insert_rowid(), payload(row)?])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, String> {
        self.conn
            .lock()
            .map_err(|_| "比较结果不可用，请重新比较".into())
    }

    fn filter(query: &DiffQuery) -> (String, Vec<rusqlite::types::Value>) {
        use rusqlite::types::Value;
        let mut clauses = vec!["1=1".to_string()];
        let mut values = Vec::new();
        if !query.include_metadata {
            clauses.push("status<>4".into());
        }
        if let Some(status) = query.status {
            clauses.push("status=?".into());
            values.push(Value::Integer(status_number(status)));
        }
        if query.focused_only {
            clauses.push("focused=1".into());
        }
        let search = query.search.trim().to_lowercase();
        if !search.is_empty() {
            clauses.push("(instr(search_path,?)>0 OR instr(search_old,?)>0)".into());
            values.extend([Value::Text(search.clone()), Value::Text(search)]);
        }
        let prefix = query.prefix.trim_matches('/');
        if !prefix.is_empty() {
            clauses.push("substr(path,1,length(?))=?".into());
            values.extend([
                Value::Text(format!("{prefix}/")),
                Value::Text(format!("{prefix}/")),
            ]);
        }
        (clauses.join(" AND "), values)
    }
    fn order(query: &DiffQuery) -> &'static str {
        match query.sort.as_str() {
            "size" => "abs_delta DESC,path",
            "time" => "seconds DESC,nanos DESC,path",
            "focus" => "focused DESC,path",
            _ => "path",
        }
    }

    pub fn query(&self, query: &DiffQuery) -> Result<DiffPage, String> {
        use rusqlite::{params_from_iter, types::Value};
        const PAGE_SIZE: usize = 100;
        let conn = self.lock()?;
        let (filter, mut values) = Self::filter(query);
        let mut folders = Vec::new();
        // SQL groups only the immediate next component. Directory rows that
        // have changed descendants join the same group, matching list semantics.
        let cte = if query.tree {
            let prefix = query.prefix.trim_matches('/');
            let base = if prefix.is_empty() {
                String::new()
            } else {
                format!("{prefix}/")
            };
            let offset = base.chars().count() + 1;
            let base_chars = base.chars().count();
            let key = format!("{filter}|{values:?}|{prefix}");
            let cached: Option<String> = sql(conn
                .query_row("SELECT value FROM tree_key", [], |r| r.get(0))
                .optional())?;
            if cached.as_deref() != Some(&key) {
                sql(conn.execute_batch("DELETE FROM tree_key; DROP TABLE IF EXISTS tree_routes; DROP TABLE IF EXISTS tree_folders;"))?;
                let routing=format!("CREATE TEMP TABLE tree_routes AS
                    WITH filtered AS (SELECT id,path,status,kind,delta,abs_delta,seconds,nanos,focused,substr(path,{offset}) relative FROM changes WHERE {filter}),
                    grouped AS (SELECT DISTINCT substr(path,1,{base_chars}+instr(relative,'/')-1) folder FROM filtered WHERE instr(relative,'/')>0)
                    SELECT *,CASE WHEN instr(relative,'/')>0 THEN substr(path,1,{base_chars}+instr(relative,'/')-1)
                        WHEN kind='directory' AND path IN (SELECT folder FROM grouped) THEN path ELSE NULL END folder FROM filtered");
                sql(conn.execute(&routing, params_from_iter(values.iter())))?;
                sql(conn.execute_batch("CREATE INDEX tree_route_folder ON tree_routes(folder);
                    CREATE TEMP TABLE tree_folders AS SELECT folder path,SUM(status=0) added,SUM(status=1) deleted,SUM(status=2) modified,
                        SUM(status=3) moved,SUM(status=4) metadata,sd_sum(delta) delta FROM tree_routes WHERE folder IS NOT NULL GROUP BY folder;
                    CREATE INDEX tree_folder_path ON tree_folders(path);"))?;
                sql(conn.execute("INSERT INTO tree_key VALUES(?1)", [key]))?;
            }
            values.clear();
            "WITH folders AS (SELECT * FROM tree_folders),direct AS (SELECT * FROM tree_routes WHERE folder IS NULL)".to_string()
        } else {
            format!("WITH direct AS (SELECT * FROM changes WHERE {filter})")
        };
        let (folder_count, direct_count): (usize, usize) = if query.tree {
            sql(conn.query_row(
                &format!(
                    "{cte} SELECT (SELECT COUNT(*) FROM folders),(SELECT COUNT(*) FROM direct)"
                ),
                params_from_iter(values.iter()),
                |r| Ok((r.get(0)?, r.get(1)?)),
            ))?
        } else {
            (
                0,
                sql(conn.query_row(
                    &format!("{cte} SELECT COUNT(*) FROM direct"),
                    params_from_iter(values.iter()),
                    |r| r.get(0),
                ))?,
            )
        };
        let total = folder_count + direct_count;
        let page = query.page.min(total.saturating_sub(1) / PAGE_SIZE);
        let start = page * PAGE_SIZE;
        if query.tree && start < folder_count {
            let mut stmt = sql(conn.prepare(&format!("{cte} SELECT path,added,deleted,modified,moved,metadata,delta FROM folders ORDER BY path LIMIT ? OFFSET ?")))?;
            let mut folder_values = values.clone();
            folder_values.extend([
                Value::Integer(PAGE_SIZE as i64),
                Value::Integer(start as i64),
            ]);
            let mut reader = sql(stmt.query(params_from_iter(folder_values)))?;
            while let Some(row) = sql(reader.next())? {
                folders.push(DiffFolder {
                    path: sql(row.get(0))?,
                    counts: DiffCounts {
                        added: sql(row.get(1))?,
                        deleted: sql(row.get(2))?,
                        modified: sql(row.get(3))?,
                        moved: sql(row.get(4))?,
                        metadata: sql(row.get(5))?,
                    },
                    size_delta: sql(row.get::<_, String>(6))?
                        .parse::<i128>()
                        .map_err(|e| e.to_string())?,
                });
            }
        }
        values.extend([
            Value::Integer((PAGE_SIZE - folders.len()) as i64),
            Value::Integer(start.saturating_sub(folder_count) as i64),
        ]);
        let order = Self::order(query);
        let projection = order
            .split(',')
            .map(|c| c.trim_end_matches(" DESC"))
            .collect::<Vec<_>>()
            .join(",");
        let outer_order = order
            .split(',')
            .map(|column| format!("p.{column}"))
            .collect::<Vec<_>>()
            .join(",");
        let mut stmt = sql(conn.prepare(&format!(
            "{cte}, page_rows AS MATERIALIZED (SELECT id,{projection} FROM direct ORDER BY {order} LIMIT ? OFFSET ?) SELECT d.payload,c.focused FROM page_rows p JOIN changes c ON c.id=p.id JOIN details d ON d.id=p.id ORDER BY {outer_order}"
        )))?;
        let mut reader = sql(stmt.query(params_from_iter(values)))?;
        let mut rows = Vec::new();
        while let Some(row) = sql(reader.next())? {
            let mut value = decode(sql(row.get(0))?)?;
            value.focused = sql(row.get(1))?;
            rows.push(value);
        }
        Ok(DiffPage {
            rows,
            folders,
            total,
            page,
            page_size: PAGE_SIZE,
        })
    }

    pub fn set_focus(&self, patterns: &[String]) -> Result<(), String> {
        let matcher = IgnoreMatcher::from_patterns(patterns)?;
        let conn = self.lock()?;
        sql(conn.create_scalar_function(
            "sd_focus",
            2,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            move |ctx| {
                Ok(
                    matcher
                        .is_ignored(&ctx.get::<String>(0)?, ctx.get::<String>(1)? == "directory"),
                )
            },
        ))?;
        // Old and new kinds can differ on a modified path.
        let result = sql(conn.execute("UPDATE changes SET focused=sd_focus(path,kind) OR (old_path<>'' AND sd_focus(old_path,old_kind))", []));
        let _ = conn.remove_function("sd_focus", 2);
        result?;
        sql(conn.execute("DELETE FROM tree_key", []))?;
        Ok(())
    }

    fn for_each(
        &self,
        query: &DiffQuery,
        mut visit: impl FnMut(DiffRow) -> Result<(), String>,
    ) -> Result<(), String> {
        let conn = self.lock()?;
        let (filter, values) = Self::filter(query);
        let mut stmt = sql(conn.prepare(&format!(
            "SELECT d.payload,focused FROM changes c JOIN details d ON d.id=c.id WHERE {filter} ORDER BY {}",
            Self::order(query)
        )))?;
        let mut rows = sql(stmt.query(rusqlite::params_from_iter(values)))?;
        while let Some(record) = sql(rows.next())? {
            let mut row = decode(sql(record.get(0))?)?;
            row.focused = sql(record.get(1))?;
            visit(row)?;
        }
        Ok(())
    }

    fn report_header(&self) -> String {
        let s = &self.summary;
        let mut text = format!("SnapDiff 目录差异报告\n目录: {}\n旧状态: {}\n新状态: {}{}\n新增 {} · 删除 {} · 修改 {} · 疑似移动 {} · 仅元数据 {}\n可比较范围大小变化: {:+} bytes\n",
            self.root_path, s.old.created_time, s.new.created_time, if s.target_current { "（当前扫描结果）" } else { "（历史快照）" },
            s.counts.added,s.counts.deleted,s.counts.modified,s.counts.moved,s.counts.metadata,s.size_delta);
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
        text
    }
    fn report_row(row: &DiffRow) -> String {
        let mut text = format!("\n[{}] {}\n", row.status.label(), row.path);
        for (label, entry) in [("旧", &row.old), ("新", &row.new)] {
            if let Some(e) = entry {
                text.push_str(&format!(
                    "  {label}: {} | {:?} | {} bytes | {} | MD5 {}\n",
                    e.relative_path, e.kind, e.size, e.modified_time, e.md5
                ));
            }
        }
        text
    }
    pub fn report_preview(&self, query: &DiffQuery) -> Result<serde_json::Value, String> {
        const LIMIT: usize = 64 * 1024;
        let mut text = String::new();
        let mut bytes = 0;
        let mut cut = false;
        let mut append = |part: &str| {
            bytes += part.len();
            if cut {
                return;
            }
            let mut len = part.len().min(LIMIT - text.len());
            while !part.is_char_boundary(len) {
                len -= 1;
            }
            text.push_str(&part[..len]);
            cut = len < part.len();
        };
        append(&self.report_header());
        self.for_each(query, |row| {
            append(&Self::report_row(&row));
            Ok(())
        })?;
        let truncated = bytes > text.len();
        if truncated {
            text.push_str("\n\n（预览已截断；保存文本文件可导出完整报告。）");
        }
        Ok(serde_json::json!({ "text":text,"bytes":bytes,"truncated":truncated }))
    }
    pub fn export_report(&self, query: &DiffQuery, path: &Path) -> Result<(), String> {
        storage::atomic_write(path, |writer| {
            writer
                .write_all(self.report_header().as_bytes())
                .map_err(|e| e.to_string())?;
            self.for_each(query, |row| {
                writer
                    .write_all(Self::report_row(&row).as_bytes())
                    .map_err(|e| e.to_string())
            })
        })
    }
    pub fn paths(&self, query: &DiffQuery, absolute: bool) -> Result<String, String> {
        let conn = self.lock()?;
        let (filter, values) = Self::filter(query);
        let mut stmt = sql(conn.prepare(&format!(
            "SELECT path FROM changes WHERE {filter} ORDER BY {}",
            Self::order(query)
        )))?;
        let mut rows = sql(stmt.query(rusqlite::params_from_iter(values)))?;
        let mut text = String::new();
        while let Some(row) = sql(rows.next())? {
            let path: String = sql(row.get(0))?;
            if !text.is_empty() {
                text.push('\n');
            }
            if absolute {
                text.push_str(&Path::new(&self.root_path).join(path).display().to_string());
            } else {
                text.push_str(&path);
            }
        }
        Ok(text)
    }
}
