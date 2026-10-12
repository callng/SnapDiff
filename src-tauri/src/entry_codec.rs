//! Internal SQLite representation; independent of the public JSON schema.
use crate::models::{EntryKind, FileEntry};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, Row};

pub const DATABASE_VERSION: u32 = 3;
pub const COLUMNS: &str = "path,kind,size,modified_seconds,modified_nanos,md5";
pub const CREATE_ENTRIES: &str = "CREATE TABLE entries(
    state_id TEXT NOT NULL REFERENCES states(id) ON DELETE CASCADE,
    path TEXT NOT NULL, kind TEXT NOT NULL,
    size INTEGER NOT NULL CHECK(size>=0),
    modified_seconds INTEGER NOT NULL,
    modified_nanos INTEGER NOT NULL CHECK(modified_nanos BETWEEN 0 AND 1999999999),
    md5 BLOB NOT NULL CHECK(length(md5) IN (0,16)),
    PRIMARY KEY(state_id,path)) WITHOUT ROWID";

pub fn hash_bytes(hash: &str) -> Result<Vec<u8>, String> {
    if hash.is_empty() {
        return Ok(vec![]);
    }
    if hash.len() != 32 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("无效 MD5 数据".into());
    }
    (0..32)
        .step_by(2)
        .map(|i| u8::from_str_radix(&hash[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

pub fn kind_name(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::File => "file",
        EntryKind::Directory => "directory",
        EntryKind::Symlink => "symlink",
    }
}

pub fn read_entry(row: &Row<'_>, offset: usize) -> rusqlite::Result<FileEntry> {
    use rusqlite::types::Type;
    let seconds: i64 = row.get(offset + 3)?;
    let nanos: u32 = row.get(offset + 4)?;
    let time = DateTime::<Utc>::from_timestamp(seconds, nanos).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            offset + 3,
            Type::Integer,
            std::io::Error::other("无效修改时间").into(),
        )
    })?;
    let hash: Vec<u8> = row.get(offset + 5)?;
    let mut md5 = String::with_capacity(hash.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in hash {
        md5.push(HEX[(byte >> 4) as usize] as char);
        md5.push(HEX[(byte & 15) as usize] as char);
    }
    Ok(FileEntry {
        relative_path: row.get(offset)?,
        kind: match row.get::<_, String>(offset + 1)?.as_str() {
            "directory" => EntryKind::Directory,
            "symlink" => EntryKind::Symlink,
            _ => EntryKind::File,
        },
        size: row.get::<_, u64>(offset + 2)?,
        modified_time: time,
        md5,
    })
}

pub fn insert_entries(
    conn: &Connection,
    table: &str,
    state: &str,
    entries: &[FileEntry],
) -> Result<(), String> {
    // table is an internal constant, never supplied by a user.
    let mut insert = conn
        .prepare_cached(&format!(
            "INSERT INTO {table}(state_id,{COLUMNS}) VALUES(?1,?2,?3,?4,?5,?6,?7)"
        ))
        .map_err(|e| e.to_string())?;
    for entry in entries {
        let size = i64::try_from(entry.size).map_err(|_| "文件大小超出数据库支持范围")?;
        insert
            .execute(params![
                state,
                entry.relative_path,
                kind_name(entry.kind),
                size,
                entry.modified_time.timestamp(),
                entry.modified_time.timestamp_subsec_nanos(),
                hash_bytes(&entry.md5)?
            ])
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn migrate(conn: &mut Connection) -> Result<(), String> {
    let transaction = conn.transaction().map_err(|e| e.to_string())?;
    let exists: bool = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='entries')",
            [],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    if exists {
        transaction
            .execute_batch("ALTER TABLE entries RENAME TO entries_v2")
            .map_err(|e| e.to_string())?;
    }
    transaction
        .execute_batch(CREATE_ENTRIES)
        .map_err(|e| e.to_string())?;
    if exists {
        let mut reader = transaction
            .prepare("SELECT state_id,path,kind,size,modified_time,md5 FROM entries_v2")
            .map_err(|e| e.to_string())?;
        let mut rows = reader.query([]).map_err(|e| e.to_string())?;
        let mut writer = transaction
            .prepare("INSERT INTO entries VALUES(?1,?2,?3,?4,?5,?6,?7)")
            .map_err(|e| e.to_string())?;
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let time =
                DateTime::parse_from_rfc3339(&row.get::<_, String>(4).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            writer
                .execute(params![
                    row.get::<_, String>(0).map_err(|e| e.to_string())?,
                    row.get::<_, String>(1).map_err(|e| e.to_string())?,
                    row.get::<_, String>(2).map_err(|e| e.to_string())?,
                    row.get::<_, i64>(3).map_err(|e| e.to_string())?,
                    time.timestamp(),
                    time.timestamp_subsec_nanos(),
                    hash_bytes(&row.get::<_, String>(5).map_err(|e| e.to_string())?)?
                ])
                .map_err(|e| e.to_string())?;
        }
        drop(writer);
        drop(rows);
        drop(reader);
        transaction
            .execute_batch("DROP TABLE entries_v2")
            .map_err(|e| e.to_string())?;
    }
    transaction
        .pragma_update(None, "user_version", DATABASE_VERSION)
        .map_err(|e| e.to_string())?;
    transaction
        .commit()
        .map_err(|e| format!("数据库升级失败（旧数据已保留）: {e}"))
}
