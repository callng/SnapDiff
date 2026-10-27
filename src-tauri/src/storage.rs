use crate::storage_location::{self, StorageLocation, DATABASE_NAME};
use crate::{entry_codec, ignore::IgnoreMatcher, models::*, sql_diff::SqlComparison};
use chrono::{Duration, Local, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::{BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Arc},
    time::Duration as StdDuration,
};
use uuid::Uuid;

fn sql<T>(result: rusqlite::Result<T>) -> Result<T, String> {
    result.map_err(|e| format!("快照数据库错误: {e}"))
}
fn encode<T: serde::Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string(value).map_err(|e| e.to_string())
}
fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, String> {
    serde_json::from_str(value).map_err(|e| format!("快照数据格式错误: {e}"))
}

pub fn folder_name(path: &str) -> String {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("目录")
        .to_string()
}

pub fn root_key(path: &str) -> String {
    let normalized = fs::canonicalize(path)
        .unwrap_or_else(|_| PathBuf::from(path))
        .to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string();
    let normalized = if let Some(unc) = normalized.strip_prefix("//?/UNC/") {
        format!("//{unc}")
    } else {
        normalized
            .strip_prefix("//?/")
            .unwrap_or(&normalized)
            .to_string()
    };
    if cfg!(windows) {
        normalized.to_lowercase()
    } else {
        normalized
    }
}

fn put_directory(conn: &Connection, config: &DirectoryConfig) -> Result<(), String> {
    sql(conn.execute(
        "INSERT INTO directories(id,root_key,config) VALUES(?1,?2,?3)
         ON CONFLICT(id) DO UPDATE SET root_key=excluded.root_key,config=excluded.config",
        params![config.id, config.root_key, encode(config)?],
    ))?;
    Ok(())
}

fn config_by_id(conn: &Connection, id: &str) -> Result<DirectoryConfig, String> {
    let value: Option<String> = sql(conn
        .query_row("SELECT config FROM directories WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .optional())?;
    decode(&value.ok_or("目录记录不存在")?)
}

fn configs(conn: &Connection) -> Result<Vec<DirectoryConfig>, String> {
    let mut stmt = sql(conn.prepare("SELECT config FROM directories ORDER BY root_key"))?;
    let values = sql(stmt.query_map([], |row| row.get::<_, String>(0)))?;
    values.map(|v| decode(&sql(v)?)).collect()
}

fn read_header(conn: &Connection, id: &str) -> Result<Snapshot, String> {
    let value: Option<String> = sql(conn
        .query_row("SELECT header FROM states WHERE id=?1", [id], |r| r.get(0))
        .optional())?;
    decode(&value.ok_or("状态记录不存在，请重新选择时间节点")?)
}

fn history(conn: &Connection, directory_id: &str) -> Result<Vec<SnapshotSummary>, String> {
    let mut stmt = sql(conn.prepare("SELECT header FROM states WHERE directory_id=?1 AND kind='snapshot' ORDER BY created_time DESC,rowid DESC"))?;
    let rows = sql(stmt.query_map([directory_id], |r| r.get::<_, String>(0)))?;
    rows.map(|v| decode::<Snapshot>(&sql(v)?).map(|s| s.summary()))
        .collect()
}

fn insert_state(conn: &Connection, snapshot: &Snapshot, saved: bool) -> Result<(), String> {
    // Store a small header separately so listing history never parses the entries.
    let header = snapshot.header();
    sql(conn.execute(
        "INSERT INTO states(id,directory_id,kind,created_time,header) VALUES(?1,?2,?3,?4,?5)",
        params![
            snapshot.id,
            snapshot.directory_id,
            if saved { "snapshot" } else { "current" },
            snapshot.created_time.timestamp_millis(),
            encode(&header)?
        ],
    ))?;
    entry_codec::insert_entries(conn, "entries", &snapshot.id, &snapshot.files)
}

fn prunable_nodes(
    nodes: &[SnapshotSummary],
    retention: usize,
) -> impl Iterator<Item = &SnapshotSummary> {
    let has_blake3 = nodes.iter().any(|node| node.hash_algorithm == "blake3");
    // Once BLAKE3 history exists, automatic retention only counts BLAKE3 nodes.
    // Legacy MD5 history remains available until the user explicitly deletes it.
    nodes
        .iter()
        .filter(move |node| retention > 0 && (!has_blake3 || node.hash_algorithm == "blake3"))
        .skip(retention)
}

fn prune(conn: &Connection, config: &mut DirectoryConfig) -> Result<usize, String> {
    if config.retention == 0 {
        return Ok(0);
    }
    let nodes = history(conn, &config.id)?;
    let removed: Vec<_> = prunable_nodes(&nodes, config.retention)
        .map(|n| n.id.clone())
        .collect();
    for id in &removed {
        sql(conn.execute("DELETE FROM states WHERE id=?1", [id]))?;
    }
    if config
        .baseline_id
        .as_ref()
        .is_some_and(|id| removed.contains(id))
    {
        config.baseline_id = nodes.first().map(|n| n.id.clone());
    }
    if config
        .current_id
        .as_ref()
        .is_some_and(|id| removed.contains(id))
    {
        config.current_id = None;
    }
    Ok(removed.len())
}

pub struct Store {
    conn: Connection,
    pub warnings: Vec<String>,
    data_dir: PathBuf,
    location: Option<StorageLocation>,
    _database_lock: fs::File,
    config_lock: Option<fs::File>,
}

impl Store {
    pub fn open(data_dir: &Path) -> Result<Self, String> {
        Self::open_internal(data_dir, true)
    }

    pub fn open_configured(location: StorageLocation) -> Result<Self, String> {
        let config_lock = storage_location::lock_file(
            location.config_file.parent().ok_or("配置目录无效")?,
            ".snapdiff-location.lock",
        )?;
        let saved = location.saved_directory()?;
        if let Some(directory) = &saved {
            if !directory.join(DATABASE_NAME).is_file() {
                return Err(format!(
                    "配置的数据库不存在或不可访问：{}。请重新连接该位置；不会自动创建空库。",
                    directory.display()
                ));
            }
        }
        let directory = saved.as_deref().unwrap_or(&location.default_dir);
        let default_location = root_key(directory.to_str().ok_or("数据库路径无效")?)
            == root_key(location.default_dir.to_str().ok_or("数据库路径无效")?);
        let mut store = if default_location {
            Self::open(directory)?
        } else {
            Self::open_internal(directory, false)?
        };
        store.location = Some(location.clone());
        store.config_lock = Some(config_lock);
        if saved.is_none() {
            // Preserve legacy history first, then migrate the complete SQLite store.
            match location.installer_directory() {
                Ok(Some(directory)) => {
                    if let Err(error) = store.migrate_to(&directory) {
                        store.warnings.push(format!(
                            "安装时选择的数据库位置未生效，继续使用原位置：{error}"
                        ));
                        location.save(&store.data_dir)?;
                    }
                }
                Ok(None) => {
                    location.save(&store.data_dir)?;
                }
                Err(error) => {
                    store.warnings.push(error);
                    location.save(&store.data_dir)?;
                }
            }
        }
        Ok(store)
    }

    fn open_internal(data_dir: &Path, migrate_legacy: bool) -> Result<Self, String> {
        let database_lock = storage_location::lock_directory(data_dir)?;
        let data_dir = fs::canonicalize(data_dir).map_err(|e| e.to_string())?;
        let mut conn = sql(Connection::open(data_dir.join(DATABASE_NAME)))?;
        let version: u32 = sql(conn.query_row("PRAGMA user_version", [], |r| r.get(0)))?;
        if version > entry_codec::DATABASE_VERSION {
            return Err("数据库来自更新版本的 SnapDiff，请使用相应版本打开".into());
        }
        sql(conn.busy_timeout(StdDuration::from_secs(5)))?;
        sql(conn.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;
             PRAGMA cache_size=-8192; PRAGMA temp_store=FILE; PRAGMA temp.cache_size=-2048;
             CREATE TABLE IF NOT EXISTS directories(id TEXT PRIMARY KEY,root_key TEXT NOT NULL UNIQUE,config TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS states(id TEXT PRIMARY KEY,directory_id TEXT NOT NULL REFERENCES directories(id) ON DELETE CASCADE,kind TEXT NOT NULL,created_time INTEGER NOT NULL,header TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS states_history ON states(directory_id,kind,created_time DESC);
             CREATE TABLE IF NOT EXISTS scan_events(id TEXT PRIMARY KEY,directory_id TEXT NOT NULL REFERENCES directories(id) ON DELETE CASCADE,local_date TEXT NOT NULL,captured INTEGER NOT NULL,changes INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS events_directory_date ON scan_events(directory_id,local_date);
             CREATE TABLE IF NOT EXISTS migrated_json(filename TEXT PRIMARY KEY);"
        ))?;
        if version < entry_codec::DATABASE_VERSION {
            entry_codec::migrate(&mut conn)?;
        }
        let mut store = Self {
            conn,
            warnings: vec![],
            data_dir,
            location: None,
            _database_lock: database_lock,
            config_lock: None,
        };
        if migrate_legacy {
            store.migrate_legacy(&store.data_dir.join("snapshots"))?;
        }
        Ok(store)
    }

    pub fn storage_info(&self) -> serde_json::Value {
        let key = root_key(&self.data_dir.to_string_lossy());
        let mode = match &self.location {
            Some(location) if key == root_key(&location.default_dir.to_string_lossy()) => "default",
            Some(location) if key == root_key(&location.install_dir.to_string_lossy()) => "install",
            _ => "custom",
        };
        let display_path = |path: &Path| {
            let text = path.to_string_lossy();
            if let Some(unc) = text.strip_prefix("\\\\?\\UNC\\") {
                format!("\\\\{unc}")
            } else {
                text.strip_prefix("\\\\?\\").unwrap_or(&text).to_string()
            }
        };
        serde_json::json!({
            "warnings": self.warnings,
            "mode": mode,
            "database_path": display_path(&self.data_dir.join(DATABASE_NAME)),
            "directory": display_path(&self.data_dir),
            "default_directory": self.location.as_ref().map(|v| display_path(&v.default_dir)),
            "install_directory": self.location.as_ref().map(|v| display_path(&v.install_dir)),
        })
    }

    pub fn ensure_database_outside_root(&self, root: &Path) -> Result<(), String> {
        let database = root_key(self.data_dir.to_str().ok_or("数据库路径无效")?);
        let root = root_key(root.to_str().ok_or("目录路径无效")?);
        if database == root || database.starts_with(&format!("{root}/")) {
            return Err(
                "该目录包含正在使用的 SnapDiff 数据库，请选择其他扫描目录或先迁移数据库".into(),
            );
        }
        Ok(())
    }

    /// The caller holds the store mutex and scan idle guard throughout this operation.
    pub fn migrate_to(&mut self, directory: &Path) -> Result<serde_json::Value, String> {
        if !directory.is_absolute() {
            return Err("请选择绝对路径的数据文件夹".into());
        }
        let location = self.location.clone().ok_or("数据库位置配置不可用")?;
        let managed_roots: Vec<_> = configs(&self.conn)?
            .into_iter()
            .map(|config| root_key(&config.root_path))
            .collect();
        let check_target = |path: &Path| -> Result<(), String> {
            let key = root_key(path.to_str().ok_or("目标路径无效")?);
            if managed_roots
                .iter()
                .any(|root| key == *root || key.starts_with(&format!("{root}/")))
            {
                return Err("数据库不能迁移到已管理的扫描目录内，请选择其他位置".into());
            }
            Ok(())
        };
        check_target(directory)?;
        check_target(&storage_location::resolve_directory(directory)?)?;
        // Canonicalize before checking managed roots, including junction aliases.
        fs::create_dir_all(directory).map_err(|e| format!("目标数据目录不可写: {e}"))?;
        let directory = fs::canonicalize(directory).map_err(|e| e.to_string())?;
        if directory == self.data_dir {
            location.save(&directory)?;
            return Ok(self.storage_info());
        }
        check_target(&directory)?;
        let target_lock = storage_location::lock_directory(&directory)?;
        let target = directory.join(DATABASE_NAME);
        for suffix in ["", "-wal", "-shm", "-journal"] {
            if fs::symlink_metadata(directory.join(format!("{DATABASE_NAME}{suffix}"))).is_ok() {
                return Err("目标文件夹已有数据库或日志文件，不会覆盖；请选择其他文件夹".into());
            }
        }
        // Reserve the final name without ever overwriting another database.
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|e| format!("创建目标数据库失败: {e}"))?;
        let result = (|| {
            let mut conn = sql(Connection::open(&target))?;
            {
                let backup = sql(rusqlite::backup::Backup::new(&self.conn, &mut conn))?;
                sql(backup.run_to_completion(256, StdDuration::from_millis(1), None))?;
            }
            let integrity: String =
                sql(conn.query_row("PRAGMA integrity_check", [], |r| r.get(0)))?;
            if integrity != "ok" {
                return Err(format!("迁移后的数据库校验失败: {integrity}"));
            }
            let violations: u64 = sql(conn.query_row(
                "SELECT count(*) FROM pragma_foreign_key_check",
                [],
                |r| r.get(0),
            ))?;
            if violations != 0 {
                return Err("迁移后的数据库引用校验失败".into());
            }
            sql(conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA cache_size=-8192; PRAGMA temp_store=FILE; PRAGMA temp.cache_size=-2048;"))?;
            sql(conn.busy_timeout(StdDuration::from_secs(5)))?;
            sql(conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)"))?;
            fs::OpenOptions::new()
                .write(true)
                .open(&target)
                .and_then(|file| file.sync_all())
                .map_err(|e| format!("保存目标数据库失败: {e}"))?;
            // This is the commit boundary. Until this succeeds the source stays active.
            location.save(&directory)?;
            Ok(conn)
        })();
        let conn = match result {
            Ok(conn) => conn,
            Err(error) => {
                for suffix in ["", "-wal", "-shm", "-journal"] {
                    let _ = fs::remove_file(directory.join(format!("{DATABASE_NAME}{suffix}")));
                }
                return Err(error);
            }
        };
        let old_directory = self.data_dir.clone();
        let destination = Self {
            conn,
            warnings: self.warnings.clone(),
            data_dir: directory,
            location: Some(location),
            _database_lock: target_lock,
            config_lock: self.config_lock.take(),
        };
        let old = std::mem::replace(self, destination);
        drop(old); // Close SQLite/WAL before deleting only this store's old database.
        let mut retained = false;
        for suffix in ["-wal", "-shm", "-journal", ""] {
            if let Err(e) = fs::remove_file(old_directory.join(format!("{DATABASE_NAME}{suffix}")))
            {
                if e.kind() != std::io::ErrorKind::NotFound {
                    retained = true;
                }
            }
        }
        let mut info = self.storage_info();
        info["source_retained"] = serde_json::json!(retained);
        Ok(info)
    }

    pub fn config(&self, id: &str) -> Result<DirectoryConfig, String> {
        config_by_id(&self.conn, id)
    }
    pub fn history(&self, id: &str) -> Result<Vec<SnapshotSummary>, String> {
        self.config(id)?;
        history(&self.conn, id)
    }

    pub fn find_root(&self, key: &str) -> Result<Option<DirectoryConfig>, String> {
        let value: Option<String> = sql(self
            .conn
            .query_row(
                "SELECT config FROM directories WHERE root_key=?1",
                [key],
                |r| r.get(0),
            )
            .optional())?;
        value.map(|v| decode(&v)).transpose()
    }

    pub fn overviews(&self) -> Result<Vec<DirectoryOverview>, String> {
        configs(&self.conn)?
            .into_iter()
            .map(|config| self.overview_config(config))
            .collect()
    }

    fn overview_config(&self, config: DirectoryConfig) -> Result<DirectoryOverview, String> {
        let history_count = sql(self.conn.query_row(
            "SELECT COUNT(*) FROM states WHERE directory_id=?1 AND kind='snapshot'",
            [&config.id],
            |r| r.get(0),
        ))?;
        let latest = self.latest_header(&config.id)?;
        let current = config
            .current_id
            .as_ref()
            .map(|id| read_header(&self.conn, id))
            .transpose()?;
        Ok(DirectoryOverview {
            available: Path::new(&config.root_path).is_dir(),
            history_count,
            latest: latest.as_ref().map(Snapshot::summary),
            current: current.as_ref().map(Snapshot::summary),
            current_types: if let Some(current) = current {
                current.type_stats
            } else if let Some(latest) = latest {
                latest.type_stats
            } else {
                BTreeMap::new()
            },
            config,
        })
    }

    pub fn overview(&self, id: &str) -> Result<DirectoryOverview, String> {
        self.overview_config(self.config(id)?)
    }

    pub fn snapshot(&self, id: &str) -> Result<Snapshot, String> {
        let mut snapshot = read_header(&self.conn, id)?;
        let mut stmt = sql(self.conn.prepare(&format!(
            "SELECT {} FROM entries WHERE state_id=?1 ORDER BY path",
            entry_codec::COLUMNS
        )))?;
        let rows = sql(stmt.query_map([id], |r| entry_codec::read_entry(r, 0)))?;
        for row in rows {
            snapshot.files.push(sql(row)?);
        }
        Ok(snapshot)
    }

    fn latest_header(&self, id: &str) -> Result<Option<Snapshot>, String> {
        let header: Option<String> = sql(self.conn.query_row(
            "SELECT header FROM states WHERE directory_id=?1 AND kind='snapshot' ORDER BY created_time DESC,rowid DESC LIMIT 1",
            [id], |r| r.get(0)
        ).optional())?;
        header.map(|h| decode(&h)).transpose()
    }

    pub fn latest_id(&self, id: &str) -> Result<Option<String>, String> {
        sql(self.conn.query_row(
            "SELECT id FROM states WHERE directory_id=?1 AND kind='snapshot' ORDER BY created_time DESC,rowid DESC LIMIT 1",
            [id], |r| r.get(0)
        ).optional())
    }

    pub fn compare_states(
        &mut self,
        old_id: &str,
        new_id: &str,
        current: bool,
        focus: &[String],
    ) -> Result<SqlComparison, String> {
        let old = read_header(&self.conn, old_id)?;
        let new = read_header(&self.conn, new_id)?;
        SqlComparison::build(&mut self.conn, &old, &new, current, focus, None, false)
    }

    pub fn scan_hash_matches(&self, old_id: &str, new: &Snapshot) -> Result<bool, String> {
        Ok(read_header(&self.conn, old_id)?.hash_algorithm() == new.hash_algorithm())
    }

    pub fn compare_scan(
        &mut self,
        old_id: &str,
        new: &Snapshot,
        current: bool,
        focus: &[String],
        cancel: Arc<AtomicBool>,
    ) -> Result<SqlComparison, String> {
        let old = read_header(&self.conn, old_id)?;
        SqlComparison::build(
            &mut self.conn,
            &old,
            new,
            current,
            focus,
            Some(cancel),
            true,
        )
    }

    pub fn ensure_saved_node(&self, directory_id: &str, id: &str) -> Result<(), String> {
        let exists: bool = sql(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM states WHERE id=?1 AND directory_id=?2 AND kind='snapshot')",
            params![id, directory_id], |r| r.get(0)))?;
        if exists {
            Ok(())
        } else {
            Err("请选择该目录的已保存时间节点".into())
        }
    }

    pub fn commit_scan(
        &mut self,
        config: &DirectoryConfig,
        snapshot: &Snapshot,
        saved: bool,
        changes: usize,
        creating: bool,
    ) -> Result<usize, String> {
        let transaction = sql(self.conn.transaction())?;
        let mut config = config.clone();
        if creating {
            let duplicate: bool = sql(transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM directories WHERE root_key=?1)",
                [&config.root_key],
                |r| r.get(0),
            ))?;
            if duplicate {
                return Err("该目录已经存在，请使用对应目录的更新按钮".into());
            }
            put_directory(&transaction, &config)?;
        } else {
            config_by_id(&transaction, &config.id)?;
        }
        insert_state(&transaction, snapshot, saved)?;
        config.current_id = Some(snapshot.id.clone());
        if saved {
            config.baseline_id = Some(snapshot.id.clone());
        }
        sql(transaction.execute(
            "DELETE FROM states WHERE directory_id=?1 AND kind='current' AND id<>?2",
            params![config.id, snapshot.id],
        ))?;
        let pruned = if saved {
            prune(&transaction, &mut config)?
        } else {
            0
        };
        put_directory(&transaction, &config)?;
        sql(transaction.execute("INSERT INTO scan_events(id,directory_id,local_date,captured,changes) VALUES(?1,?2,?3,?4,?5)",
            params![snapshot.id, config.id, snapshot.created_time.with_timezone(&Local).format("%Y-%m-%d").to_string(), saved, changes as i64]))?;
        sql(transaction.commit())?;
        Ok(pruned)
    }

    pub fn select_baseline(&mut self, directory_id: &str, id: &str) -> Result<(), String> {
        self.ensure_saved_node(directory_id, id)?;
        let mut config = self.config(directory_id)?;
        config.baseline_id = Some(id.into());
        put_directory(&self.conn, &config)
    }

    pub fn retention_preview(
        &self,
        id: &str,
        retention: usize,
    ) -> Result<Vec<SnapshotSummary>, String> {
        if retention > 100_000 {
            return Err("保留数量不能超过 100000".into());
        }
        let nodes = self.history(id)?;
        Ok(prunable_nodes(&nodes, retention).cloned().collect())
    }

    pub fn update_settings(
        &mut self,
        id: &str,
        ignore: Vec<String>,
        focus: Vec<String>,
        retention: usize,
    ) -> Result<usize, String> {
        IgnoreMatcher::from_patterns(&ignore)?;
        IgnoreMatcher::from_patterns(&focus)?;
        self.retention_preview(id, retention)?;
        let transaction = sql(self.conn.transaction())?;
        let mut config = config_by_id(&transaction, id)?;
        config.ignore_patterns = ignore;
        config.focus_patterns = focus;
        config.retention = retention;
        let pruned = prune(&transaction, &mut config)?;
        put_directory(&transaction, &config)?;
        sql(transaction.commit())?;
        Ok(pruned)
    }

    pub fn set_focus_patterns(&mut self, id: &str, focus: Vec<String>) -> Result<(), String> {
        IgnoreMatcher::from_patterns(&focus)?;
        let mut config = self.config(id)?;
        config.focus_patterns = focus;
        // A focus change must never execute the history retention policy.
        put_directory(&self.conn, &config)
    }

    pub fn delete_nodes(&mut self, id: &str, ids: &[String]) -> Result<(), String> {
        for node in ids {
            self.ensure_saved_node(id, node)?;
        }
        let transaction = sql(self.conn.transaction())?;
        let mut config = config_by_id(&transaction, id)?;
        for node in ids {
            sql(transaction.execute("DELETE FROM states WHERE id=?1", [node]))?;
        }
        let nodes = history(&transaction, id)?;
        if config
            .baseline_id
            .as_ref()
            .is_some_and(|node| ids.contains(node))
        {
            config.baseline_id = nodes.first().map(|n| n.id.clone());
        }
        if config
            .current_id
            .as_ref()
            .is_some_and(|node| ids.contains(node))
        {
            config.current_id = None;
        }
        put_directory(&transaction, &config)?;
        sql(transaction.commit())
    }

    pub fn set_remark(&mut self, directory_id: &str, id: &str, remark: &str) -> Result<(), String> {
        self.ensure_saved_node(directory_id, id)?;
        if remark.chars().count() > 500 {
            return Err("备注最多 500 个字符".into());
        }
        let mut header = read_header(&self.conn, id)?;
        header.remark = remark.trim().into();
        sql(self.conn.execute(
            "UPDATE states SET header=?1 WHERE id=?2",
            params![encode(&header)?, id],
        ))?;
        Ok(())
    }

    pub fn delete_directory(&mut self, id: &str) -> Result<(), String> {
        self.config(id)?;
        sql(self
            .conn
            .execute("DELETE FROM directories WHERE id=?1", [id]))?;
        Ok(())
    }

    pub fn activity(&self, id: &str) -> Result<Vec<ActivityDay>, String> {
        self.config(id)?;
        let today = Local::now().date_naive();
        let mut days: BTreeMap<String, ActivityDay> = (0..120)
            .map(|offset| {
                let date = (today - Duration::days(offset))
                    .format("%Y-%m-%d")
                    .to_string();
                (
                    date.clone(),
                    ActivityDay {
                        date,
                        scans: 0,
                        captures: 0,
                        changes: 0,
                    },
                )
            })
            .collect();
        let start = days.keys().next().cloned().unwrap();
        let mut stmt = sql(self.conn.prepare("SELECT local_date,COUNT(*),SUM(captured),SUM(changes) FROM scan_events WHERE directory_id=?1 AND local_date>=?2 GROUP BY local_date"))?;
        let rows = sql(stmt.query_map(params![id, start], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        }))?;
        for row in rows {
            let (date, scans, captures, changes) = sql(row)?;
            if let Some(day) = days.get_mut(&date) {
                day.scans = scans as usize;
                day.captures = captures as usize;
                day.changes = changes as usize;
            }
        }
        Ok(days.into_values().collect())
    }

    fn migrate_legacy(&mut self, folder: &Path) -> Result<(), String> {
        if !folder.is_dir() {
            return Ok(());
        }
        let mut pending = Vec::new();
        for entry in fs::read_dir(folder).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let filename = path.file_name().unwrap().to_string_lossy().to_string();
            let migrated: bool = sql(self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM migrated_json WHERE filename=?1)",
                [&filename],
                |r| r.get(0),
            ))?;
            if migrated {
                continue;
            }
            let loaded = (|| -> Result<Snapshot, String> {
                let file = fs::File::open(&path).map_err(|e| e.to_string())?;
                let mut snapshot: Snapshot =
                    serde_json::from_reader(BufReader::new(file)).map_err(|e| e.to_string())?;
                prepare_snapshot(&mut snapshot)?;
                Ok(snapshot)
            })();
            let snapshot = match loaded {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    self.warnings.push(format!("{}: {error}", path.display()));
                    continue;
                }
            };
            pending.push((filename, snapshot));
        }
        if pending.is_empty() {
            return Ok(());
        }
        let transaction = sql(self.conn.transaction())?;
        let mut by_key: HashMap<String, DirectoryConfig> = configs(&transaction)?
            .into_iter()
            .map(|d| (d.root_key.clone(), d))
            .collect();
        let mut affected = HashSet::new();
        for (filename, mut snapshot) in pending {
            let key = root_key(&snapshot.root_path);
            let config = by_key
                .entry(key.clone())
                .or_insert_with(|| new_config(&snapshot.root_path, &snapshot.ignore_patterns));
            put_directory(&transaction, config)?;
            snapshot.directory_id = config.id.clone();
            let exists: bool = sql(transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM states WHERE id=?1)",
                [&snapshot.id],
                |r| r.get(0),
            ))?;
            if !exists {
                insert_state(&transaction, &snapshot, true)?;
            }
            affected.insert(config.id.clone());
            sql(transaction.execute("INSERT INTO migrated_json(filename) VALUES(?1)", [filename]))?;
        }
        for config in by_key.values_mut().filter(|d| affected.contains(&d.id)) {
            if config.baseline_id.is_none() {
                config.baseline_id = history(&transaction, &config.id)?
                    .first()
                    .map(|n| n.id.clone());
            }
            put_directory(&transaction, config)?;
        }
        sql(transaction.commit())
    }

    #[cfg(test)]
    pub fn archive(
        &self,
        directory_id: Option<&str>,
        ids: &[String],
    ) -> Result<SnapshotArchive, String> {
        let directories = if let Some(id) = directory_id {
            vec![self.config(id)?]
        } else {
            configs(&self.conn)?
        };
        let mut snapshots = Vec::new();
        for directory in &directories {
            for node in self.history(&directory.id)? {
                if ids.is_empty() || ids.contains(&node.id) {
                    snapshots.push(self.snapshot(&node.id)?);
                }
            }
        }
        if snapshots.is_empty() {
            return Err("没有可导出的已保存快照".into());
        }
        if !ids.is_empty() && snapshots.len() != ids.iter().collect::<HashSet<_>>().len() {
            return Err("选择的时间节点不存在".into());
        }
        Ok(SnapshotArchive {
            format: "snapdiff-archive".into(),
            schema_version: CURRENT_SCHEMA_VERSION,
            exported_time: Utc::now(),
            directories,
            snapshots,
        })
    }

    pub fn export_archive(
        &self,
        directory_id: Option<&str>,
        ids: &[String],
        path: &Path,
    ) -> Result<(), String> {
        let directories = if let Some(id) = directory_id {
            vec![self.config(id)?]
        } else {
            configs(&self.conn)?
        };
        let requested: HashSet<_> = ids.iter().collect();
        let mut nodes = Vec::new();
        for directory in &directories {
            let mut stmt=sql(self.conn.prepare("SELECT id FROM states WHERE directory_id=?1 AND kind='snapshot' ORDER BY created_time DESC,rowid DESC"))?;
            for id in sql(stmt.query_map([&directory.id], |r| r.get::<_, String>(0)))? {
                let id = sql(id)?;
                if requested.is_empty() || requested.contains(&id) {
                    nodes.push(id);
                }
            }
        }
        if nodes.is_empty() {
            return Err("没有可导出的已保存快照".into());
        }
        if !requested.is_empty() && requested.len() != nodes.len() {
            return Err("选择的时间节点不存在".into());
        }
        atomic_write(path, |writer| {
            let mut header = serde_json::to_value(SnapshotArchive {
                format: "snapdiff-archive".into(),
                schema_version: CURRENT_SCHEMA_VERSION,
                exported_time: Utc::now(),
                directories,
                snapshots: vec![],
            })
            .map_err(|e| e.to_string())?;
            header.as_object_mut().unwrap().remove("snapshots");
            let header = encode(&header)?;
            writer
                .write_all(&header.as_bytes()[..header.len() - 1])
                .map_err(|e| e.to_string())?;
            writer
                .write_all(b",\"snapshots\":[")
                .map_err(|e| e.to_string())?;
            for (index, id) in nodes.iter().enumerate() {
                if index > 0 {
                    writer.write_all(b",").map_err(|e| e.to_string())?;
                }
                let mut header = serde_json::to_value(read_header(&self.conn, id)?)
                    .map_err(|e| e.to_string())?;
                header.as_object_mut().unwrap().remove("files");
                let header = encode(&header)?;
                writer
                    .write_all(&header.as_bytes()[..header.len() - 1])
                    .map_err(|e| e.to_string())?;
                writer
                    .write_all(b",\"files\":[")
                    .map_err(|e| e.to_string())?;
                let mut stmt = sql(self.conn.prepare(&format!(
                    "SELECT {} FROM entries WHERE state_id=?1 ORDER BY path",
                    entry_codec::COLUMNS
                )))?;
                for (index, entry) in
                    sql(stmt.query_map([id], |r| entry_codec::read_entry(r, 0)))?.enumerate()
                {
                    if index > 0 {
                        writer.write_all(b",").map_err(|e| e.to_string())?;
                    }
                    serde_json::to_writer(&mut *writer, &sql(entry)?).map_err(|e| e.to_string())?;
                }
                writer.write_all(b"]}").map_err(|e| e.to_string())?;
            }
            writer.write_all(b"]}").map_err(|e| e.to_string())
        })
    }

    pub fn preview_import(&self, archive: &SnapshotArchive) -> Result<ImportPreview, String> {
        validate_archive(archive)?;
        let mut duplicates = 0;
        for snapshot in &archive.snapshots {
            let exists: bool = sql(self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM states WHERE id=?1)",
                [&snapshot.id],
                |r| r.get(0),
            ))?;
            if exists {
                let existing = self.snapshot(&snapshot.id)?;
                // IDs are immutable. A conflicting import must never overwrite local history.
                if existing.root_path != snapshot.root_path
                    || existing.created_time != snapshot.created_time
                    || existing.files != snapshot.files
                {
                    return Err(format!("快照 ID 冲突: {}，本地历史未修改", snapshot.id));
                }
                duplicates += 1;
            }
        }
        let roots = archive
            .snapshots
            .iter()
            .map(|s| s.root_path.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        Ok(ImportPreview {
            directories: roots.len(),
            snapshots: archive.snapshots.len(),
            new_snapshots: archive.snapshots.len() - duplicates,
            duplicates,
            roots,
        })
    }

    pub fn import_archive(&mut self, archive: &SnapshotArchive) -> Result<ImportPreview, String> {
        let preview = self.preview_import(archive)?;
        let transaction = sql(self.conn.transaction())?;
        let mut by_key: HashMap<String, DirectoryConfig> = configs(&transaction)?
            .into_iter()
            .map(|d| (d.root_key.clone(), d))
            .collect();
        let mut affected = HashSet::new();
        for original in &archive.snapshots {
            let mut snapshot = original.clone();
            prepare_snapshot(&mut snapshot)?;
            let key = root_key(&snapshot.root_path);
            let config = by_key.entry(key).or_insert_with(|| {
                let mut config = new_config(&snapshot.root_path, &snapshot.ignore_patterns);
                if let Some(exported) = archive
                    .directories
                    .iter()
                    .find(|d| d.id == original.directory_id)
                {
                    config.ignore_patterns = exported.ignore_patterns.clone();
                    config.focus_patterns = exported.focus_patterns.clone();
                    config.retention = exported.retention;
                }
                config
            });
            put_directory(&transaction, config)?;
            snapshot.directory_id = config.id.clone();
            let exists: bool = sql(transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM states WHERE id=?1)",
                [&snapshot.id],
                |r| r.get(0),
            ))?;
            if !exists {
                insert_state(&transaction, &snapshot, true)?;
            }
            affected.insert(config.id.clone());
        }
        for config in by_key.values_mut().filter(|d| affected.contains(&d.id)) {
            if config.baseline_id.is_none() {
                config.baseline_id = history(&transaction, &config.id)?
                    .first()
                    .map(|n| n.id.clone());
            }
            // Import preserves all history. Retention only applies to the next explicit capture/settings action.
            put_directory(&transaction, config)?;
        }
        sql(transaction.commit())?;
        Ok(preview)
    }
}

pub fn new_config(root: &str, ignore: &[String]) -> DirectoryConfig {
    DirectoryConfig {
        id: Uuid::new_v4().to_string(),
        name: folder_name(root),
        root_path: root.into(),
        root_key: root_key(root),
        baseline_id: None,
        current_id: None,
        ignore_patterns: ignore.into(),
        retention: 0,
        focus_patterns: vec![],
    }
}

fn prepare_snapshot(snapshot: &mut Snapshot) -> Result<(), String> {
    if snapshot.schema_version == 0 || snapshot.schema_version > CURRENT_SCHEMA_VERSION {
        return Err("不支持的快照版本".into());
    }
    if snapshot.id.is_empty() || snapshot.root_path.trim().is_empty() {
        return Err("快照缺少 ID 或目录路径".into());
    }
    if snapshot.remark.chars().count() > 500 {
        return Err("快照备注过长".into());
    }
    let mut paths = HashSet::new();
    for entry in &mut snapshot.files {
        let path = &entry.relative_path;
        if path.is_empty()
            || path.starts_with('/')
            || path.contains(['\\', ':', '\0'])
            || path
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(format!("快照包含无效相对路径: {path}"));
        }
        if !paths.insert(path) {
            return Err(format!("快照包含重复路径: {path}"));
        }
        if entry.kind == EntryKind::Directory {
            if entry.size != 0 || !entry.hash.is_empty() {
                return Err(format!("目录条目格式错误: {path}"));
            }
        } else if entry.hash.len() != if snapshot.schema_version >= 3 { 64 } else { 32 }
            || !entry.hash.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(format!("快照包含无效摘要: {path}"));
        }
        entry.hash.make_ascii_lowercase();
    }
    IgnoreMatcher::from_patterns(&snapshot.ignore_patterns)?;
    if snapshot.schema_version == 1 && snapshot.effective_ignore_patterns.is_empty() {
        snapshot.effective_ignore_patterns = snapshot.ignore_patterns.clone();
    }
    IgnoreMatcher::from_patterns(&snapshot.effective_ignore_patterns)?;
    snapshot
        .files
        .sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    snapshot.recalculate_stats()
}

fn validate_archive(archive: &SnapshotArchive) -> Result<(), String> {
    if archive.format != "snapdiff-archive" || !matches!(archive.schema_version, 2 | 3) {
        return Err("不支持的快照数据格式".into());
    }
    if archive.snapshots.is_empty() {
        return Err("导入文件中没有快照".into());
    }
    let mut ids = HashSet::new();
    for snapshot in &archive.snapshots {
        if !ids.insert(&snapshot.id) {
            return Err("导入文件中存在重复快照 ID".into());
        }
        let mut snapshot = snapshot.clone();
        prepare_snapshot(&mut snapshot)?;
    }
    for directory in &archive.directories {
        IgnoreMatcher::from_patterns(&directory.ignore_patterns)?;
        IgnoreMatcher::from_patterns(&directory.focus_patterns)?;
        if directory.retention > 100_000 {
            return Err("导入数据中的历史保留设置无效".into());
        }
    }
    Ok(())
}

pub fn read_archive(path: &Path) -> Result<SnapshotArchive, String> {
    let file = fs::File::open(path).map_err(|e| format!("无法读取导入文件: {e}"))?;
    let value: serde_json::Value =
        serde_json::from_reader(BufReader::new(file)).map_err(|e| format!("JSON 格式错误: {e}"))?;
    let mut archive = if value.get("format").is_some() {
        serde_json::from_value::<SnapshotArchive>(value).map_err(|e| e.to_string())?
    } else {
        let snapshot: Snapshot =
            serde_json::from_value(value).map_err(|e| format!("快照格式错误: {e}"))?;
        SnapshotArchive {
            format: "snapdiff-archive".into(),
            schema_version: CURRENT_SCHEMA_VERSION,
            exported_time: Utc::now(),
            directories: vec![],
            snapshots: vec![snapshot],
        }
    };
    for snapshot in &mut archive.snapshots {
        prepare_snapshot(snapshot)?;
    }
    validate_archive(&archive)?;
    Ok(archive)
}

#[cfg(test)]
pub fn write_archive(path: &Path, archive: &SnapshotArchive) -> Result<(), String> {
    atomic_write(path, |writer| {
        serde_json::to_writer(writer, archive).map_err(|e| e.to_string())
    })
}

pub(crate) fn atomic_write(
    path: &Path,
    write: impl FnOnce(&mut BufWriter<fs::File>) -> Result<(), String>,
) -> Result<(), String> {
    let parent = path.parent().ok_or("保存路径无效")?;
    let temporary = parent.join(format!(".snapdiff-{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let file = fs::File::create(&temporary).map_err(|e| e.to_string())?;
        let mut writer = BufWriter::new(file);
        write(&mut writer)?;
        writer.flush().map_err(|e| e.to_string())?;
        writer.get_ref().sync_all().map_err(|e| e.to_string())?;
        drop(writer);
        fs::rename(&temporary, path).map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|e| format!("保存文件失败: {e}"))
}

#[cfg(test)]
#[path = "storage_location_tests.rs"]
mod location_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff;
    fn store() -> (tempfile::TempDir, Store) {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        (temp, store)
    }
    fn capture(store: &mut Store, config: &DirectoryConfig, id: &str, saved: bool, creating: bool) {
        let mut snapshot = diff::test_snapshot(id, vec![]);
        snapshot.directory_id = config.id.clone();
        snapshot.root_path = config.root_path.clone();
        store
            .commit_scan(config, &snapshot, saved, 0, creating)
            .unwrap();
    }
    #[test]
    fn current_checks_do_not_create_history_and_directory_survives_empty_history() {
        let (_temp, mut store) = store();
        let config = new_config("C:/folder", &[]);
        capture(&mut store, &config, "first", true, true);
        let config = store.config(&config.id).unwrap();
        capture(&mut store, &config, "check1", false, false);
        let config = store.config(&config.id).unwrap();
        capture(&mut store, &config, "check2", false, false);
        assert_eq!(store.history(&config.id).unwrap().len(), 1);
        assert_eq!(
            store.config(&config.id).unwrap().baseline_id.as_deref(),
            Some("first")
        );
        assert!(store.snapshot("check1").is_err());
        store.delete_nodes(&config.id, &["first".into()]).unwrap();
        assert_eq!(store.overview(&config.id).unwrap().history_count, 0);
        assert_eq!(
            store.config(&config.id).unwrap().current_id.as_deref(),
            Some("check2")
        );
    }
    #[test]
    fn retention_reassigns_baseline_and_duplicate_roots_are_rejected() {
        let (_temp, mut store) = store();
        let config = new_config("C:/folder", &[]);
        capture(&mut store, &config, "one", true, true);
        let config = store.config(&config.id).unwrap();
        capture(&mut store, &config, "two", true, false);
        assert_eq!(store.retention_preview(&config.id, 1).unwrap().len(), 1);
        store
            .update_settings(&config.id, vec![], vec![], 1)
            .unwrap();
        let latest = store.history(&config.id).unwrap();
        assert_eq!(latest.len(), 1);
        assert_eq!(
            store.config(&config.id).unwrap().baseline_id.as_deref(),
            Some(latest[0].id.as_str())
        );
        let duplicate = new_config("C:/folder", &[]);
        let mut snapshot = diff::test_snapshot("duplicate", vec![]);
        snapshot.directory_id = duplicate.id.clone();
        assert!(store
            .commit_scan(&duplicate, &snapshot, true, 0, true)
            .is_err());
        assert_eq!(store.overviews().unwrap().len(), 1);
    }
    #[test]
    fn legacy_json_is_migrated_once_and_original_files_remain() {
        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join("snapshots");
        fs::create_dir(&legacy).unwrap();
        let value = r#"{"schema_version":1,"id":"old","name":"Custom","created_time":"2026-10-12T00:00:00Z","root_path":"C:/folder","file_count":1,"files":[{"relative_path":"a.txt","size":3,"modified_time":"2026-10-12T00:00:00Z","md5":"900150983cd24fb0d6963f7d28e17f72"}]}"#;
        fs::write(legacy.join("old.json"), value).unwrap();
        let store = Store::open(temp.path()).unwrap();
        let id = store.overviews().unwrap()[0].config.id.clone();
        assert_eq!(store.overview(&id).unwrap().config.name, "folder");
        assert_eq!(store.snapshot("old").unwrap().total_size, 3);
        assert!(!store.snapshot("old").unwrap().directory_entries_complete);
        drop(store);
        let mut store = Store::open(temp.path()).unwrap();
        assert_eq!(store.history(&id).unwrap().len(), 1);
        store.delete_nodes(&id, &["old".into()]).unwrap();
        drop(store);
        assert_eq!(
            Store::open(temp.path())
                .unwrap()
                .history(&id)
                .unwrap()
                .len(),
            0
        );
        assert_eq!(fs::read_to_string(legacy.join("old.json")).unwrap(), value);
    }
    #[test]
    fn archive_roundtrip_and_conflicts_never_overwrite_history() {
        let (_temp, mut source) = store();
        let config = new_config("C:/folder", &[]);
        capture(&mut source, &config, "node", true, true);
        let archive = source.archive(Some(&config.id), &[]).unwrap();
        let (_other, mut target) = store();
        assert_eq!(target.import_archive(&archive).unwrap().new_snapshots, 1);
        assert_eq!(target.import_archive(&archive).unwrap().duplicates, 1);
        let mut conflict = archive.clone();
        conflict.snapshots[0].created_time += Duration::seconds(1);
        assert!(target.import_archive(&conflict).is_err());
        assert_eq!(
            target.snapshot("node").unwrap().created_time,
            archive.snapshots[0].created_time
        );
        let mut invalid = archive;
        invalid.snapshots[0].files.push(FileEntry {
            relative_path: "../escape".into(),
            kind: EntryKind::File,
            size: 0,
            modified_time: Utc::now(),
            hash: "a".repeat(32),
        });
        assert!(target.import_archive(&invalid).is_err());
    }
    #[test]
    fn failed_transaction_does_not_leave_an_empty_directory() {
        let (_temp, mut store) = store();
        let config = new_config("C:/folder", &[]);
        let mut snapshot = diff::test_snapshot("node", vec![]);
        snapshot.directory_id = "wrong".into();
        assert!(store
            .commit_scan(&config, &snapshot, true, 0, true)
            .is_err());
        assert!(store.overviews().unwrap().is_empty());
    }

    fn downgrade_entries(store: &Store, snapshot: &Snapshot, invalid: bool) {
        store.conn.execute_batch("DROP TABLE entries;
            CREATE TABLE entries(state_id TEXT NOT NULL REFERENCES states(id) ON DELETE CASCADE,path TEXT NOT NULL,kind TEXT NOT NULL,size INTEGER NOT NULL,modified_time TEXT NOT NULL,md5 TEXT NOT NULL,PRIMARY KEY(state_id,path));
            PRAGMA user_version=2;").unwrap();
        let mut writer = store
            .conn
            .prepare("INSERT INTO entries VALUES(?1,?2,?3,?4,?5,?6)")
            .unwrap();
        for (i, e) in snapshot.files.iter().enumerate() {
            writer
                .execute(params![
                    snapshot.id,
                    e.relative_path,
                    entry_codec::kind_name(e.kind),
                    e.size as i64,
                    e.modified_time.to_rfc3339(),
                    if invalid && i == 1 {
                        "bad".into()
                    } else {
                        e.hash.clone()
                    }
                ])
                .unwrap();
        }
    }

    #[test]
    fn sqlite_v2_upgrade_preserves_nanoseconds_old_dates_and_cascade() {
        let (temp, mut store) = store();
        let config = new_config("C:/folder", &[]);
        let mut snapshot = diff::test_snapshot(
            "legacy-db",
            vec![
                FileEntry {
                    relative_path: "old-file".into(),
                    kind: EntryKind::File,
                    size: 42,
                    modified_time: chrono::DateTime::parse_from_rfc3339(
                        "1600-01-02T03:04:05.123456789Z",
                    )
                    .unwrap()
                    .with_timezone(&Utc),
                    hash: "abcd".repeat(8),
                },
                FileEntry {
                    relative_path: "empty".into(),
                    kind: EntryKind::Directory,
                    size: 0,
                    modified_time: chrono::DateTime::parse_from_rfc3339(
                        "1969-12-31T23:59:59.999999999Z",
                    )
                    .unwrap()
                    .with_timezone(&Utc),
                    hash: String::new(),
                },
            ],
        );
        snapshot.directory_id = config.id.clone();
        store
            .commit_scan(&config, &snapshot, true, 0, true)
            .unwrap();
        let config_before = serde_json::to_value(store.config(&config.id).unwrap()).unwrap();
        downgrade_entries(&store, &snapshot, false);
        drop(store);
        let mut store = Store::open(temp.path()).unwrap();
        let mut expected = snapshot.files.clone();
        expected.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        assert_eq!(store.snapshot("legacy-db").unwrap().files, expected);
        assert_eq!(
            serde_json::to_value(store.config(&config.id).unwrap()).unwrap(),
            config_before
        );
        assert_eq!(
            store
                .conn
                .query_row::<u32, _, _>("PRAGMA user_version", [], |r| r.get(0))
                .unwrap(),
            entry_codec::DATABASE_VERSION
        );
        assert_eq!(
            store
                .conn
                .query_row::<String, _, _>(
                    "SELECT typeof(hash) FROM entries WHERE path='old-file'",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
            "blob"
        );
        assert_eq!(
            store
                .conn
                .query_row::<usize, _, _>(
                    "SELECT length(hash) FROM entries WHERE path='old-file'",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
            16
        );
        // Reopening does not rerun migration or change timestamps.
        drop(store);
        store = Store::open(temp.path()).unwrap();
        assert_eq!(store.snapshot("legacy-db").unwrap().files, expected);
        store.delete_directory(&config.id).unwrap();
        assert_eq!(
            store
                .conn
                .query_row::<usize, _, _>("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn v3_md5_history_survives_upgrade_and_blake3_capture_roundtrip() {
        let (temp, mut store) = store();
        let config = new_config("C:/offline-history", &[]);
        let old_entry = FileEntry {
            relative_path: "a.txt".into(),
            kind: EntryKind::File,
            size: 3,
            modified_time: Utc::now(),
            hash: "900150983cd24fb0d6963f7d28e17f72".into(),
        };
        let mut old = diff::test_snapshot("md5-old", vec![old_entry]);
        old.directory_id = config.id.clone();
        store.commit_scan(&config, &old, true, 0, true).unwrap();
        let mut later = old.clone();
        later.id = "md5-later".into();
        later.created_time += Duration::seconds(1);
        store.commit_scan(&config, &later, true, 0, false).unwrap();
        let header_before: String = store
            .conn
            .query_row("SELECT header FROM states WHERE id='md5-old'", [], |r| {
                r.get(0)
            })
            .unwrap();
        store.conn.execute_batch("ALTER TABLE entries RENAME TO entries_new;
            CREATE TABLE entries(state_id TEXT NOT NULL REFERENCES states(id) ON DELETE CASCADE,
              path TEXT NOT NULL,kind TEXT NOT NULL,size INTEGER NOT NULL CHECK(size>=0),
              modified_seconds INTEGER NOT NULL,modified_nanos INTEGER NOT NULL,
              md5 BLOB NOT NULL CHECK(length(md5) IN (0,16)),PRIMARY KEY(state_id,path)) WITHOUT ROWID;
            INSERT INTO entries SELECT * FROM entries_new; DROP TABLE entries_new; PRAGMA user_version=3;").unwrap();
        drop(store);
        let mut store = Store::open(temp.path()).unwrap();
        assert_eq!(store.snapshot("md5-old").unwrap().files, old.files);
        assert_eq!(store.history(&config.id).unwrap().len(), 2);
        let header_after: String = store
            .conn
            .query_row("SELECT header FROM states WHERE id='md5-old'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(header_before, header_after);
        assert_eq!(
            store
                .compare_states("md5-old", "md5-later", false, &[])
                .unwrap()
                .row_count,
            0
        );
        let mut new = later.clone();
        new.id = "blake3-base".into();
        new.schema_version = CURRENT_SCHEMA_VERSION;
        new.created_time += Duration::seconds(1);
        new.files[0].hash = blake3::hash(b"abc").to_hex().to_string();
        assert!(!store.scan_hash_matches("md5-old", &new).unwrap());
        assert!(store
            .compare_scan(
                "md5-old",
                &new,
                false,
                &[],
                Arc::new(AtomicBool::new(false))
            )
            .err()
            .unwrap()
            .contains("不能比较"));
        store.commit_scan(&config, &new, true, 0, false).unwrap();
        assert_eq!(
            store.config(&config.id).unwrap().baseline_id.as_deref(),
            Some("blake3-base")
        );
        let mut next = new.clone();
        next.id = "blake3-new".into();
        next.created_time += Duration::seconds(1);
        next.files[0].hash = blake3::hash(b"def").to_hex().to_string();
        store.commit_scan(&config, &next, true, 1, false).unwrap();
        assert_eq!(
            store
                .compare_states("blake3-base", "blake3-new", false, &[])
                .unwrap()
                .summary
                .counts
                .modified,
            1
        );
        assert!(diff::compare(&old, &new, false, &[])
            .unwrap_err()
            .contains("不能比较"));
        let hash_size: usize = store
            .conn
            .query_row(
                "SELECT length(hash) FROM entries WHERE state_id='blake3-base'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hash_size, 32);
        let out = tempfile::tempdir().unwrap();
        let path = out.path().join("mixed.json");
        store.export_archive(Some(&config.id), &[], &path).unwrap();
        let archive = read_archive(&path).unwrap();
        assert_eq!(archive.schema_version, 3);
        assert_eq!(archive.snapshots.len(), 4);
        let target = tempfile::tempdir().unwrap();
        let mut imported = Store::open(target.path()).unwrap();
        imported.import_archive(&archive).unwrap();
        assert_eq!(imported.snapshot("md5-old").unwrap().files, old.files);
        assert_eq!(imported.snapshot("blake3-base").unwrap().files, new.files);
        assert_eq!(imported.import_archive(&archive).unwrap().duplicates, 4);
        drop(store);
        let mut reopened = Store::open(temp.path()).unwrap();
        assert_eq!(reopened.snapshot("md5-old").unwrap().files, old.files);
        assert_eq!(reopened.snapshot("blake3-new").unwrap().files, next.files);
        let preview = reopened.retention_preview(&config.id, 1).unwrap();
        assert_eq!(preview.len(), 1);
        assert_eq!(preview[0].id, "blake3-base");
        assert_eq!(
            reopened
                .update_settings(&config.id, vec![], vec![], 1)
                .unwrap(),
            1
        );
        assert_eq!(reopened.history(&config.id).unwrap().len(), 3);
        next.id = "blake3-final".into();
        next.created_time += Duration::seconds(1);
        let config = reopened.config(&config.id).unwrap();
        assert_eq!(
            reopened
                .commit_scan(&config, &next, true, 0, false)
                .unwrap(),
            1
        );
        assert_eq!(reopened.history(&config.id).unwrap().len(), 3);
        assert_eq!(reopened.snapshot("md5-old").unwrap().files, old.files);
        assert_eq!(reopened.snapshot("md5-later").unwrap().files, later.files);
    }

    #[test]
    fn first_blake3_capture_with_latest_retention_preserves_md5_history() {
        let (_temp, mut store) = store();
        let mut config = new_config("C:/offline-transition", &[]);
        config.retention = 1;
        let mut old = diff::test_snapshot(
            "old-md5",
            vec![FileEntry {
                relative_path: "a.txt".into(),
                kind: EntryKind::File,
                size: 3,
                modified_time: Utc::now(),
                hash: "900150983cd24fb0d6963f7d28e17f72".into(),
            }],
        );
        old.directory_id = config.id.clone();
        store.commit_scan(&config, &old, true, 0, true).unwrap();
        let mut new = old.clone();
        new.id = "blake3-first".into();
        new.schema_version = CURRENT_SCHEMA_VERSION;
        new.created_time += Duration::seconds(1);
        new.files[0].hash = blake3::hash(b"abc").to_hex().to_string();
        assert_eq!(store.commit_scan(&config, &new, true, 0, false).unwrap(), 0);
        assert_eq!(store.history(&config.id).unwrap().len(), 2);
        assert!(store.retention_preview(&config.id, 1).unwrap().is_empty());
        new.id = "blake3-next".into();
        new.created_time += Duration::seconds(1);
        assert_eq!(store.commit_scan(&config, &new, true, 0, false).unwrap(), 1);
        assert_eq!(store.history(&config.id).unwrap().len(), 2);
        assert_eq!(store.snapshot("old-md5").unwrap().files, old.files);
    }

    #[test]
    fn public_hash_schema_rejects_wrong_algorithm_length_and_preserves_legacy_json() {
        let mut new = diff::test_snapshot(
            "new",
            vec![FileEntry {
                relative_path: "a".into(),
                kind: EntryKind::File,
                size: 3,
                modified_time: Utc::now(),
                hash: blake3::hash(b"abc").to_hex().to_string(),
            }],
        );
        prepare_snapshot(&mut new).unwrap();
        let mut invalid = new.clone();
        invalid.files[0].hash = "a".repeat(32);
        assert!(prepare_snapshot(&mut invalid).is_err());
        invalid = new.clone();
        invalid.files[0].hash = "z".repeat(64);
        assert!(prepare_snapshot(&mut invalid).is_err());
        invalid = new.clone();
        invalid.schema_version = 2;
        assert!(prepare_snapshot(&mut invalid).is_err());
        let mut json = serde_json::to_value(&new).unwrap();
        json["schema_version"] = 2.into();
        json["files"][0].as_object_mut().unwrap().remove("hash");
        json["files"][0]["md5"] = "900150983cd24fb0d6963f7d28e17f72".into();
        let mut old: Snapshot = serde_json::from_value(json).unwrap();
        prepare_snapshot(&mut old).unwrap();
        assert_eq!(old.hash_algorithm(), "md5");
        assert_eq!(old.files[0].hash, "900150983cd24fb0d6963f7d28e17f72");
        assert!(serde_json::to_value(&new).unwrap()["files"][0]
            .get("md5")
            .is_none());
    }

    #[test]
    fn failed_sqlite_upgrade_rolls_back_and_newer_versions_are_rejected() {
        let (temp, mut store) = store();
        let config = new_config("C:/folder", &[]);
        let entry = FileEntry {
            relative_path: "a".into(),
            kind: EntryKind::File,
            size: 42,
            modified_time: Utc::now(),
            hash: "a".repeat(32),
        };
        let mut other = entry.clone();
        other.relative_path = "b".into();
        let mut snapshot = diff::test_snapshot("legacy-db", vec![entry, other]);
        snapshot.directory_id = config.id.clone();
        store
            .commit_scan(&config, &snapshot, true, 0, true)
            .unwrap();
        downgrade_entries(&store, &snapshot, true);
        drop(store);
        assert!(Store::open(temp.path()).is_err());
        let conn = Connection::open(temp.path().join("snapdiff.sqlite3")).unwrap();
        assert_eq!(
            conn.query_row::<u32, _, _>("PRAGMA user_version", [], |r| r.get(0))
                .unwrap(),
            2
        );
        assert_eq!(
            conn.query_row::<usize, _, _>(
                "SELECT COUNT(*) FROM entries WHERE modified_time<>''",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            2
        );
        assert_eq!(
            conn.query_row::<String, _, _>("SELECT md5 FROM entries WHERE path='b'", [], |r| r
                .get(0))
                .unwrap(),
            "bad"
        );
        conn.pragma_update(None, "user_version", 99).unwrap();
        drop(conn);
        assert!(Store::open(temp.path()).err().unwrap().contains("更新版本"));
    }

    #[test]
    fn real_directory_scan_check_capture_and_archive_lifecycle() {
        use crate::scanner;
        use std::sync::{atomic::AtomicBool, Arc};
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("empty")).unwrap();
        fs::write(root.path().join("change.txt"), "before").unwrap();
        fs::write(root.path().join("remove.txt"), "delete").unwrap();
        fs::write(root.path().join("old.txt"), "rename").unwrap();
        fs::write(root.path().join(".snapshotignore"), "*.log\n").unwrap();
        fs::write(root.path().join("ignored.log"), "noise").unwrap();
        let (_data, mut store) = store();
        let config = new_config(root.path().to_str().unwrap(), &[]);
        let scan = |id: &str| {
            let ignore = IgnoreMatcher::from_root(root.path(), &[]).unwrap();
            let files = scanner::scan_directory(
                root.path(),
                &ignore,
                &Arc::new(AtomicBool::new(false)),
                |_| {},
            )
            .unwrap();
            let mut state = diff::test_snapshot(id, files);
            state.directory_id = config.id.clone();
            state.root_path = config.root_path.clone();
            state.effective_ignore_patterns = ignore.effective_patterns;
            state
        };
        let first = scan("first");
        store.commit_scan(&config, &first, true, 0, true).unwrap();
        fs::write(root.path().join("change.txt"), "after").unwrap();
        fs::remove_file(root.path().join("remove.txt")).unwrap();
        fs::rename(root.path().join("old.txt"), root.path().join("new.txt")).unwrap();
        fs::write(root.path().join("added.bin"), "added").unwrap();
        let current = scan("check");
        let comparison = diff::compare(&first, &current, true, &[]).unwrap();
        let sql_comparison = store
            .compare_scan(
                "first",
                &current,
                true,
                &[],
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        assert_eq!(
            serde_json::to_value(sql_comparison.query(&DiffQuery::default()).unwrap()).unwrap(),
            serde_json::to_value(diff::query(&comparison, &DiffQuery::default())).unwrap()
        );
        assert_eq!(
            (
                comparison.summary.counts.added,
                comparison.summary.counts.deleted,
                comparison.summary.counts.modified,
                comparison.summary.counts.moved
            ),
            (1, 1, 1, 1)
        );
        let config = store.config(&config.id).unwrap();
        store
            .commit_scan(&config, &current, false, 4, false)
            .unwrap();
        assert_eq!(store.history(&config.id).unwrap().len(), 1);
        assert!(store
            .snapshot("check")
            .unwrap()
            .files
            .iter()
            .any(|e| e.kind == EntryKind::Directory));
        let config = store.config(&config.id).unwrap();
        let mut saved = current;
        saved.id = "saved".into();
        store.commit_scan(&config, &saved, true, 4, false).unwrap();
        assert_eq!(store.history(&config.id).unwrap().len(), 2);
        assert!(store.snapshot("check").is_err());
        let archive_path = root.path().join("export.json");
        write_archive(
            &archive_path,
            &store.archive(Some(&config.id), &[]).unwrap(),
        )
        .unwrap();
        let archive = read_archive(&archive_path).unwrap();
        assert_eq!(archive.snapshots.len(), 2);
        let (_target_data, mut target) = self::store();
        target.import_archive(&archive).unwrap();
        let offline = target.overviews().unwrap().remove(0);
        let historical = diff::compare(
            &target.snapshot("first").unwrap(),
            &target.snapshot("saved").unwrap(),
            false,
            &[],
        )
        .unwrap();
        assert_eq!(historical.summary.counts.content_total(), 4);
        assert!(offline.current.is_none());
        let days = target.activity(&offline.config.id).unwrap();
        assert!(days.iter().all(|day| day.scans == 0));
    }

    #[test]
    fn corrupt_legacy_json_is_reported_without_blocking_valid_history() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join("snapshots");
        fs::create_dir(&legacy).unwrap();
        fs::write(legacy.join("broken.json"), "{broken").unwrap();
        let store = Store::open(root.path()).unwrap();
        assert_eq!(store.warnings.len(), 1);
        assert!(store.overviews().unwrap().is_empty());
        assert_eq!(
            fs::read_to_string(legacy.join("broken.json")).unwrap(),
            "{broken"
        );
    }

    #[test]
    fn focus_changes_preserve_history_imported_beyond_the_retention_limit() {
        let (_source_dir, mut source) = store();
        let config = new_config("C:/folder", &[]);
        capture(&mut source, &config, "one", true, true);
        let config = source.config(&config.id).unwrap();
        capture(&mut source, &config, "two", true, false);
        let mut archive = source.archive(Some(&config.id), &[]).unwrap();
        archive.directories[0].retention = 1;
        let (_target_dir, mut target) = store();
        target.import_archive(&archive).unwrap();
        let directory = target.overviews().unwrap().remove(0);
        assert_eq!(directory.config.retention, 1);
        target
            .set_focus_patterns(&directory.config.id, vec!["*.ini".into()])
            .unwrap();
        assert_eq!(target.history(&directory.config.id).unwrap().len(), 2);
    }
}
