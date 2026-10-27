use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const CURRENT_SCHEMA_VERSION: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    #[default]
    File,
    Directory,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub relative_path: String,
    #[serde(default)]
    pub kind: EntryKind,
    pub size: u64,
    pub modified_time: DateTime<Utc>,
    #[serde(alias = "md5")]
    pub hash: String,
}

impl FileEntry {
    pub fn hash_label(&self) -> &'static str {
        if self.hash.len() == 32 {
            "MD5（历史）"
        } else {
            "BLAKE3"
        }
    }
}

// Schema 1/2 JSON stays readable. New captures record their actual scan scope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema_version: u32,
    pub id: String,
    #[serde(default)]
    pub directory_id: String,
    pub name: String,
    pub created_time: DateTime<Utc>,
    #[serde(default)]
    pub scan_started_time: Option<DateTime<Utc>>,
    pub root_path: String,
    #[serde(default)]
    pub ignore_patterns: Vec<String>,
    #[serde(default)]
    pub effective_ignore_patterns: Vec<String>,
    #[serde(default)]
    pub directory_entries_complete: bool,
    #[serde(default)]
    pub remark: String,
    pub file_count: usize,
    #[serde(default)]
    pub directory_count: usize,
    #[serde(default)]
    pub total_size: u64,
    #[serde(default)]
    pub type_stats: BTreeMap<String, TypeStat>,
    pub files: Vec<FileEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TypeStat {
    pub count: usize,
    pub size: u64,
}

impl Snapshot {
    pub fn hash_algorithm(&self) -> &'static str {
        if self.schema_version >= 3 {
            "blake3"
        } else {
            "md5"
        }
    }
    pub fn ensure_comparable(&self, other: &Self) -> Result<(), String> {
        if self.hash_algorithm() != other.hash_algorithm() {
            return Err(
                "MD5 历史与 BLAKE3 状态不能比较内容，请更新快照建立 BLAKE3 基准；旧历史会保留"
                    .into(),
            );
        }
        Ok(())
    }
    pub fn header(&self) -> Self {
        Self {
            schema_version: self.schema_version,
            id: self.id.clone(),
            directory_id: self.directory_id.clone(),
            name: self.name.clone(),
            created_time: self.created_time,
            scan_started_time: self.scan_started_time,
            root_path: self.root_path.clone(),
            ignore_patterns: self.ignore_patterns.clone(),
            effective_ignore_patterns: self.effective_ignore_patterns.clone(),
            directory_entries_complete: self.directory_entries_complete,
            remark: self.remark.clone(),
            file_count: self.file_count,
            directory_count: self.directory_count,
            total_size: self.total_size,
            type_stats: self.type_stats.clone(),
            files: vec![],
        }
    }
    pub fn recalculate_stats(&mut self) -> Result<(), String> {
        self.file_count = 0;
        self.directory_count = 0;
        self.total_size = 0;
        self.type_stats.clear();
        for entry in &self.files {
            if entry.kind == EntryKind::Directory {
                self.directory_count += 1;
                continue;
            }
            self.file_count += 1;
            self.total_size = self
                .total_size
                .checked_add(entry.size)
                .ok_or("文件总大小超出范围")?;
            let name = entry.relative_path.rsplit('/').next().unwrap_or("");
            let extension = if entry.kind == EntryKind::Symlink {
                "链接".to_string()
            } else {
                name.rsplit_once('.')
                    .filter(|(stem, _)| !stem.is_empty())
                    .map(|(_, ext)| format!(".{}", ext.to_lowercase()))
                    .unwrap_or_else(|| "无扩展名".into())
            };
            let stat = self.type_stats.entry(extension).or_default();
            stat.count += 1;
            stat.size += entry.size;
        }
        Ok(())
    }
    pub fn summary(&self) -> SnapshotSummary {
        SnapshotSummary {
            hash_algorithm: self.hash_algorithm().into(),
            id: self.id.clone(),
            directory_id: self.directory_id.clone(),
            created_time: self.created_time,
            scan_started_time: self.scan_started_time,
            file_count: self.file_count,
            directory_count: self.directory_count,
            total_size: self.total_size,
            remark: self.remark.clone(),
            directory_entries_complete: self.directory_entries_complete,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotSummary {
    pub hash_algorithm: String,
    pub id: String,
    pub directory_id: String,
    pub created_time: DateTime<Utc>,
    pub scan_started_time: Option<DateTime<Utc>>,
    pub file_count: usize,
    pub directory_count: usize,
    pub total_size: u64,
    pub remark: String,
    pub directory_entries_complete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectoryConfig {
    pub id: String,
    pub name: String,
    pub root_path: String,
    pub root_key: String,
    pub baseline_id: Option<String>,
    pub current_id: Option<String>,
    pub ignore_patterns: Vec<String>,
    // 0 = unlimited; a positive number limits saved nodes.
    pub retention: usize,
    pub focus_patterns: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DirectoryOverview {
    #[serde(flatten)]
    pub config: DirectoryConfig,
    pub history_count: usize,
    pub latest: Option<SnapshotSummary>,
    pub current: Option<SnapshotSummary>,
    pub current_types: BTreeMap<String, TypeStat>,
    pub available: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ScanProgress {
    pub task_id: String,
    pub phase: String,
    pub processed: usize,
    pub total: usize,
    pub current_path: String,
    pub current_file_bytes: u64,
    pub current_file_size: u64,
    pub processed_bytes: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffStatus {
    Added,
    Deleted,
    Modified,
    Moved,
    Metadata,
}

impl DiffStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Added => "新增",
            Self::Deleted => "删除",
            Self::Modified => "修改",
            Self::Moved => "疑似移动 / 重命名",
            Self::Metadata => "仅元数据变化",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffRow {
    pub path: String,
    pub status: DiffStatus,
    pub old: Option<FileEntry>,
    pub new: Option<FileEntry>,
    pub size_delta: i128,
    pub focused: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DiffCounts {
    pub added: usize,
    pub deleted: usize,
    pub modified: usize,
    pub moved: usize,
    pub metadata: usize,
}

impl DiffCounts {
    #[cfg(test)]
    pub fn add(&mut self, status: DiffStatus) {
        match status {
            DiffStatus::Added => self.added += 1,
            DiffStatus::Deleted => self.deleted += 1,
            DiffStatus::Modified => self.modified += 1,
            DiffStatus::Moved => self.moved += 1,
            DiffStatus::Metadata => self.metadata += 1,
        }
    }
    pub fn content_total(&self) -> usize {
        self.added + self.deleted + self.modified + self.moved
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ComparisonSummary {
    pub id: String,
    pub directory_id: String,
    pub old: SnapshotSummary,
    pub new: SnapshotSummary,
    pub target_current: bool,
    pub counts: DiffCounts,
    pub size_delta: i128,
    pub scope_changed: bool,
    pub scope_unknown: bool,
    pub excluded_scope_count: usize,
    pub directory_structure_available: bool,
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub struct Comparison {
    pub summary: ComparisonSummary,
    pub root_path: String,
    pub rows: Vec<DiffRow>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffQuery {
    #[serde(default)]
    pub search: String,
    pub status: Option<DiffStatus>,
    #[serde(default)]
    pub include_metadata: bool,
    #[serde(default)]
    pub focused_only: bool,
    #[serde(default)]
    pub sort: String,
    #[serde(default)]
    pub tree: bool,
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub page: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffFolder {
    pub path: String,
    pub counts: DiffCounts,
    pub size_delta: i128,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffPage {
    pub rows: Vec<DiffRow>,
    pub folders: Vec<DiffFolder>,
    pub total: usize,
    pub page: usize,
    pub page_size: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActivityDay {
    pub date: String,
    pub scans: usize,
    pub captures: usize,
    pub changes: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanResult {
    pub notice: Option<String>,
    pub directory_id: String,
    pub state: SnapshotSummary,
    pub comparison: Option<ComparisonSummary>,
    pub saved: bool,
    pub pruned: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotArchive {
    pub format: String,
    pub schema_version: u32,
    pub exported_time: DateTime<Utc>,
    pub directories: Vec<DirectoryConfig>,
    pub snapshots: Vec<Snapshot>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportPreview {
    pub directories: usize,
    pub snapshots: usize,
    pub new_snapshots: usize,
    pub duplicates: usize,
    pub roots: Vec<String>,
}
