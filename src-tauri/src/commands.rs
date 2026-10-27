use crate::{
    ignore::IgnoreMatcher,
    models::*,
    scanner,
    sql_diff::SqlComparison,
    storage::{self, Store},
};
use chrono::Utc;
use serde::Deserialize;
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
};
use tauri::{AppHandle, Emitter, State};
use uuid::Uuid;

fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>, String> {
    mutex
        .lock()
        .map_err(|_| "应用状态不可用，请重新启动".into())
}

#[derive(Default, Clone)]
pub struct ScanState {
    active: Arc<Mutex<Option<ActiveScan>>>,
}
struct ActiveScan {
    id: String,
    token: Arc<AtomicBool>,
    saving: bool,
}
struct ScanJob {
    id: String,
    token: Arc<AtomicBool>,
    active: Arc<Mutex<Option<ActiveScan>>>,
}

impl ScanState {
    fn begin(&self, id: String) -> Result<ScanJob, String> {
        Uuid::parse_str(&id).map_err(|_| "扫描任务 ID 无效")?;
        let mut active = lock(&self.active)?;
        if active.is_some() {
            return Err("已有扫描任务正在进行".into());
        }
        let token = Arc::new(AtomicBool::new(false));
        *active = Some(ActiveScan {
            id: id.clone(),
            token: token.clone(),
            saving: false,
        });
        Ok(ScanJob {
            id,
            token,
            active: self.active.clone(),
        })
    }
    fn idle_guard(&self) -> Result<MutexGuard<'_, Option<ActiveScan>>, String> {
        let guard = lock(&self.active)?;
        if guard.is_some() {
            return Err("扫描期间暂不能修改目录或历史，请等待完成或取消扫描".into());
        }
        Ok(guard)
    }
    fn cancel(&self, id: &str) -> Result<(), String> {
        let active = lock(&self.active)?;
        match active.as_ref() {
            Some(scan) if scan.id != id => Err("该扫描任务已结束".into()),
            Some(scan) if scan.saving => Err("扫描已结束，正在提交完整状态".into()),
            Some(scan) => {
                scan.token.store(true, Ordering::Relaxed);
                Ok(())
            }
            None => Ok(()),
        }
    }
}

impl ScanJob {
    fn checkpoint(&self) -> Result<(), String> {
        if self.token.load(Ordering::Relaxed) {
            Err(scanner::SCAN_CANCELLED.into())
        } else {
            Ok(())
        }
    }
    fn start_saving(&self) -> Result<(), String> {
        let mut active = lock(&self.active)?;
        self.checkpoint()?;
        if let Some(scan) = active.as_mut() {
            scan.saving = true;
        }
        Ok(())
    }
    fn emit(&self, app: &AppHandle, mut progress: ScanProgress) {
        progress.task_id = self.id.clone();
        let _ = app.emit("scan-progress", progress);
    }
}
impl Drop for ScanJob {
    fn drop(&mut self) {
        if let Ok(mut active) = self.active.lock() {
            if active.as_ref().is_some_and(|scan| scan.id == self.id) {
                *active = None;
            }
        }
    }
}

#[derive(Default)]
struct ComparisonCache {
    entries: HashMap<String, Arc<SqlComparison>>,
    order: VecDeque<String>,
}
impl ComparisonCache {
    fn put(&mut self, comparison: SqlComparison) -> ComparisonSummary {
        let summary = comparison.summary.clone();
        self.order.push_back(summary.id.clone());
        self.entries
            .insert(summary.id.clone(), Arc::new(comparison));
        while self.order.len() > 8
            || (self.order.len() > 1
                && self.entries.values().map(|c| c.row_count).sum::<usize>() > 250_000)
        {
            if let Some(id) = self.order.pop_front() {
                self.entries.remove(&id);
            }
        }
        summary
    }
}

#[derive(Clone)]
pub struct Backend {
    pub store: Arc<Mutex<Store>>,
    scan: ScanState,
    comparisons: Arc<Mutex<ComparisonCache>>,
    imports: Arc<Mutex<HashMap<String, Arc<SnapshotArchive>>>>,
}
impl Backend {
    pub fn new(store: Store) -> Self {
        Self {
            store: Arc::new(Mutex::new(store)),
            scan: ScanState::default(),
            comparisons: Arc::new(Mutex::new(ComparisonCache::default())),
            imports: Arc::new(Mutex::new(HashMap::new())),
        }
    }
    fn comparison(&self, id: &str) -> Result<Arc<SqlComparison>, String> {
        lock(&self.comparisons)?
            .entries
            .get(id)
            .cloned()
            .ok_or_else(|| "比较结果已过期，请重新比较".into())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanRequest {
    pub task_id: String,
    pub mode: String,
    pub directory_id: Option<String>,
    pub root_path: Option<String>,
    #[serde(default)]
    pub ignore_patterns: Vec<String>,
    #[serde(default)]
    pub remark: String,
}

fn run_scan(
    app: AppHandle,
    backend: Backend,
    job: ScanJob,
    request: ScanRequest,
) -> Result<ScanResult, String> {
    if !["create", "check", "capture"].contains(&request.mode.as_str()) {
        return Err("扫描模式无效".into());
    }
    if request.remark.chars().count() > 500 {
        return Err("备注最多 500 个字符".into());
    }
    let creating = request.mode == "create";
    let saved = request.mode != "check";
    let (config, old_id) = {
        let store = lock(&backend.store)?;
        if creating {
            let root = request.root_path.as_deref().ok_or("请选择目录")?;
            let key = storage::root_key(root);
            if store.find_root(&key)?.is_some() {
                return Err("该目录已经存在，请使用对应目录的更新按钮".into());
            }
            (storage::new_config(root, &request.ignore_patterns), None)
        } else {
            let id = request.directory_id.as_deref().ok_or("请选择目录")?;
            let config = store.config(id)?;
            let old = if saved {
                store.latest_id(id)?
            } else {
                config.baseline_id.clone()
            };
            (config, old)
        }
    };
    let root = PathBuf::from(&config.root_path);
    lock(&backend.store)?.ensure_database_outside_root(&root)?;
    if !root.is_dir() {
        return Err("目录不存在或不可访问；已有历史仍可离线比较".into());
    }
    let started = Utc::now();
    let matcher = IgnoreMatcher::from_root(&root, &config.ignore_patterns)?;
    let files = scanner::scan_directory(&root, &matcher, &job.token, |progress| {
        job.emit(&app, progress)
    })?;
    let mut snapshot = Snapshot {
        schema_version: CURRENT_SCHEMA_VERSION,
        id: Uuid::new_v4().to_string(),
        directory_id: config.id.clone(),
        name: config.name.clone(),
        root_path: config.root_path.clone(),
        created_time: Utc::now(),
        scan_started_time: Some(started),
        ignore_patterns: config.ignore_patterns.clone(),
        effective_ignore_patterns: matcher.effective_patterns,
        directory_entries_complete: true,
        remark: request.remark.trim().into(),
        file_count: 0,
        directory_count: 0,
        total_size: 0,
        type_stats: Default::default(),
        files,
    };
    snapshot.recalculate_stats()?;
    job.checkpoint()?;
    job.emit(
        &app,
        ScanProgress {
            phase: "comparing".into(),
            ..Default::default()
        },
    );
    let mut notice = None;
    let old_id = if let Some(id) = old_id {
        if lock(&backend.store)?.scan_hash_matches(&id, &snapshot)? {
            Some(id)
        } else {
            notice = Some(if saved {
                "已保存 BLAKE3 基准，原 MD5 历史保留；不同算法不生成内容差异，下次扫描起可正常比较。"
            } else {
                "本次已使用 BLAKE3 扫描，原 MD5 历史保留；请更新快照建立 BLAKE3 基准后再比较。"
            }.to_string());
            None
        }
    } else {
        None
    };
    let comparison = if let Some(id) = old_id {
        job.checkpoint()?;
        Some(lock(&backend.store)?.compare_scan(
            &id,
            &snapshot,
            !saved,
            &config.focus_patterns,
            job.token.clone(),
        )?)
    } else {
        None
    };
    job.checkpoint()?;
    let changes = comparison
        .as_ref()
        .map_or(0, |c| c.summary.counts.content_total());
    // No directory/history mutation is visible until the whole transaction commits.
    job.start_saving()?;
    job.emit(
        &app,
        ScanProgress {
            phase: "saving".into(),
            ..Default::default()
        },
    );
    let pruned = lock(&backend.store)?.commit_scan(&config, &snapshot, saved, changes, creating)?;
    let comparison = comparison
        .map(|c| lock(&backend.comparisons).map(|mut cache| cache.put(c)))
        .transpose()?;
    job.emit(
        &app,
        ScanProgress {
            phase: "completed".into(),
            ..Default::default()
        },
    );
    Ok(ScanResult {
        notice,
        directory_id: config.id,
        state: snapshot.summary(),
        comparison,
        saved,
        pruned,
    })
}

#[tauri::command]
pub async fn scan_directory(
    app: AppHandle,
    state: State<'_, Backend>,
    request: ScanRequest,
) -> Result<ScanResult, String> {
    let backend = state.inner().clone();
    let job = backend.scan.begin(request.task_id.clone())?;
    tauri::async_runtime::spawn_blocking(move || run_scan(app, backend, job, request))
        .await
        .map_err(|e| format!("扫描任务失败: {e}"))?
}

#[tauri::command]
pub fn cancel_scan(state: State<'_, Backend>, task_id: String) -> Result<(), String> {
    state.scan.cancel(&task_id)
}

#[tauri::command]
pub async fn list_directories(state: State<'_, Backend>) -> Result<Vec<DirectoryOverview>, String> {
    let backend = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || lock(&backend.store)?.overviews())
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn get_directory(
    state: State<'_, Backend>,
    directory_id: String,
) -> Result<DirectoryOverview, String> {
    let backend = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || lock(&backend.store)?.overview(&directory_id))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn list_history(
    state: State<'_, Backend>,
    directory_id: String,
) -> Result<Vec<SnapshotSummary>, String> {
    let backend = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || lock(&backend.store)?.history(&directory_id))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn select_baseline(
    state: State<'_, Backend>,
    directory_id: String,
    snapshot_id: String,
) -> Result<(), String> {
    let backend = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = backend.scan.idle_guard()?;
        lock(&backend.store)?.select_baseline(&directory_id, &snapshot_id)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn compare_states(
    state: State<'_, Backend>,
    directory_id: String,
    old_id: String,
    new_id: Option<String>,
) -> Result<ComparisonSummary, String> {
    let backend = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let comparison = {
            let mut store = lock(&backend.store)?;
            let config = store.config(&directory_id)?;
            store.ensure_saved_node(&directory_id, &old_id)?;
            let next = match &new_id {
                Some(id) => {
                    store.ensure_saved_node(&directory_id, id)?;
                    id.clone()
                }
                None => config
                    .current_id
                    .ok_or("还没有当前扫描状态，请点击检查变化")?,
            };
            if new_id.is_some() && next == old_id {
                return Err("请选择两个不同的时间节点".into());
            }
            // Preserve left-to-right direction explicitly; chronology is displayed in the UI.
            store.compare_states(&old_id, &next, new_id.is_none(), &config.focus_patterns)?
        };
        Ok(lock(&backend.comparisons)?.put(comparison))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn query_diff(
    state: State<'_, Backend>,
    comparison_id: String,
    query: DiffQuery,
) -> Result<DiffPage, String> {
    let comparison = state.comparison(&comparison_id)?;
    tauri::async_runtime::spawn_blocking(move || comparison.query(&query))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn get_activity(
    state: State<'_, Backend>,
    directory_id: String,
) -> Result<Vec<ActivityDay>, String> {
    let backend = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || lock(&backend.store)?.activity(&directory_id))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn preview_retention(
    state: State<'_, Backend>,
    directory_id: String,
    retention: usize,
) -> Result<Vec<SnapshotSummary>, String> {
    let backend = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        lock(&backend.store)?.retention_preview(&directory_id, retention)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn update_settings(
    state: State<'_, Backend>,
    directory_id: String,
    ignore_patterns: Vec<String>,
    focus_patterns: Vec<String>,
    retention: usize,
) -> Result<usize, String> {
    let backend = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = backend.scan.idle_guard()?;
        lock(&backend.store)?.update_settings(
            &directory_id,
            ignore_patterns,
            focus_patterns,
            retention,
        )
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn delete_nodes(
    state: State<'_, Backend>,
    directory_id: String,
    snapshot_ids: Vec<String>,
) -> Result<(), String> {
    let backend = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = backend.scan.idle_guard()?;
        lock(&backend.store)?.delete_nodes(&directory_id, &snapshot_ids)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn set_focus_patterns(
    state: State<'_, Backend>,
    directory_id: String,
    focus_patterns: Vec<String>,
) -> Result<(), String> {
    let backend = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = backend.scan.idle_guard()?;
        IgnoreMatcher::from_patterns(&focus_patterns)?;
        lock(&backend.store)?.set_focus_patterns(&directory_id, focus_patterns.clone())?;
        let cached: Vec<_> = lock(&backend.comparisons)?
            .entries
            .values()
            .cloned()
            .collect();
        for comparison in cached {
            if comparison.summary.directory_id != directory_id {
                continue;
            }
            comparison.set_focus(&focus_patterns)?;
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn set_remark(
    state: State<'_, Backend>,
    directory_id: String,
    snapshot_id: String,
    remark: String,
) -> Result<(), String> {
    let backend = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = backend.scan.idle_guard()?;
        lock(&backend.store)?.set_remark(&directory_id, &snapshot_id, &remark)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn delete_directory(
    state: State<'_, Backend>,
    directory_id: String,
) -> Result<(), String> {
    let backend = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = backend.scan.idle_guard()?;
        lock(&backend.store)?.delete_directory(&directory_id)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn get_report(
    state: State<'_, Backend>,
    comparison_id: String,
    query: DiffQuery,
) -> Result<serde_json::Value, String> {
    let comparison = state.comparison(&comparison_id)?;
    tauri::async_runtime::spawn_blocking(move || comparison.report_preview(&query))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn get_storage_info(state: State<'_, Backend>) -> Result<serde_json::Value, String> {
    let backend = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || Ok(lock(&backend.store)?.storage_info()))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn migrate_database(
    state: State<'_, Backend>,
    directory: String,
) -> Result<serde_json::Value, String> {
    let backend = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = backend.scan.idle_guard()?;
        lock(&backend.store)?.migrate_to(Path::new(&directory))
    })
    .await
    .map_err(|e| e.to_string())?
}

fn validate_new_directory(store: &Store, path: &str) -> Result<(), String> {
    if !Path::new(path).is_dir() {
        return Err("请拖入一个可访问的文件夹，不能拖入文件".into());
    }
    if store.find_root(&storage::root_key(path))?.is_some() {
        return Err("该目录已经存在，请使用对应目录的更新按钮".into());
    }
    store.ensure_database_outside_root(Path::new(path))?;
    Ok(())
}

/// Validate a native dropped path without scanning or saving anything.
#[tauri::command]
pub async fn validate_directory_path(
    state: State<'_, Backend>,
    path: String,
) -> Result<String, String> {
    let backend = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = backend.scan.idle_guard()?;
        let store = lock(&backend.store)?;
        validate_new_directory(&store, &path)?;
        Ok(path)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn export_report(
    state: State<'_, Backend>,
    comparison_id: String,
    query: DiffQuery,
    path: String,
) -> Result<(), String> {
    let comparison = state.comparison(&comparison_id)?;
    tauri::async_runtime::spawn_blocking(move || comparison.export_report(&query, Path::new(&path)))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn get_diff_paths(
    state: State<'_, Backend>,
    comparison_id: String,
    query: DiffQuery,
    absolute: bool,
) -> Result<String, String> {
    let comparison = state.comparison(&comparison_id)?;
    tauri::async_runtime::spawn_blocking(move || comparison.paths(&query, absolute))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn export_snapshots(
    state: State<'_, Backend>,
    directory_id: Option<String>,
    snapshot_ids: Vec<String>,
    path: String,
) -> Result<(), String> {
    let backend = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        lock(&backend.store)?.export_archive(
            directory_id.as_deref(),
            &snapshot_ids,
            Path::new(&path),
        )
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn preview_import(
    state: State<'_, Backend>,
    path: String,
) -> Result<serde_json::Value, String> {
    let backend = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let archive = storage::read_archive(Path::new(&path))?;
        let preview = lock(&backend.store)?.preview_import(&archive)?;
        let ticket = Uuid::new_v4().to_string();
        let mut imports = lock(&backend.imports)?;
        // Keep the exact previewed data so changing a file before confirmation cannot change the import.
        if imports.len() >= 2 {
            imports.clear();
        }
        imports.insert(ticket.clone(), Arc::new(archive));
        Ok(serde_json::json!({ "ticket_id": ticket, "preview": preview }))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn import_snapshots(
    state: State<'_, Backend>,
    ticket_id: String,
) -> Result<ImportPreview, String> {
    let backend = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = backend.scan.idle_guard()?;
        let archive = lock(&backend.imports)?
            .get(&ticket_id)
            .cloned()
            .ok_or("导入预览已过期，请重新选择文件")?;
        let result = lock(&backend.store)?.import_archive(&archive)?;
        lock(&backend.imports)?.remove(&ticket_id);
        Ok(result)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dropped_paths_require_a_new_accessible_directory_without_writing_records() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(data.path()).unwrap();
        let path = root.path().to_str().unwrap();
        assert!(validate_new_directory(&store, path).is_ok());
        assert!(store.find_root(&storage::root_key(path)).unwrap().is_none());
        let file = root.path().join("file.txt");
        std::fs::write(&file, "test").unwrap();
        assert!(validate_new_directory(&store, file.to_str().unwrap()).is_err());
        assert!(
            validate_new_directory(&store, root.path().join("missing").to_str().unwrap()).is_err()
        );
        let config = storage::new_config(path, &[]);
        // Creating a directory row exercises canonical-path duplicate detection.
        let connection = rusqlite::Connection::open(data.path().join("snapdiff.sqlite3")).unwrap();
        connection
            .execute(
                "INSERT INTO directories(id,root_key,config) VALUES(?1,?2,?3)",
                rusqlite::params![
                    config.id,
                    config.root_key,
                    serde_json::to_string(&config).unwrap()
                ],
            )
            .unwrap();
        assert!(
            validate_new_directory(&store, root.path().join(".").to_str().unwrap())
                .unwrap_err()
                .contains("已经存在")
        );
    }
    #[test]
    fn cancellation_and_commit_boundary_are_task_scoped() {
        let state = ScanState::default();
        let id = Uuid::new_v4().to_string();
        let job = state.begin(id.clone()).unwrap();
        assert!(state.begin(Uuid::new_v4().to_string()).is_err());
        assert!(state.cancel("another-task").is_err());
        assert!(job.checkpoint().is_ok());
        state.cancel(&id).unwrap();
        assert_eq!(job.start_saving().unwrap_err(), scanner::SCAN_CANCELLED);
        drop(job);
        let job = state.begin(Uuid::new_v4().to_string()).unwrap();
        job.start_saving().unwrap();
        assert!(state.cancel(&job.id).is_err());
        assert!(state.idle_guard().is_err());
        drop(job);
        assert!(state.idle_guard().is_ok());
    }
}
