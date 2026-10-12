'use strict';
SD.app = (() => {
  const { $, state: s, invoke, esc, date, bytes, toast, action } = SD;
  let initialized = false, readyPromise, directoryReadFailed = false, dropRequest = 0;

  async function refreshDirectories() {
    try {
      s.directories = await invoke('list_directories') || [];
      $('directory-count').textContent = '(' + s.directories.length + ')';
      $('directory-list').innerHTML = s.directories.map(directory => {
        const current = directory.current || directory.latest;
        return '<article class="directory-card"><button class="directory-open" data-open="' + esc(directory.id) + '"><span class="directory-icon">▱</span><span class="directory-info"><strong>' + esc(directory.name) + '</strong><span class="path" title="' + esc(directory.root_path) + '">' + esc(directory.root_path) + '</span><small>' + (current ? current.file_count.toLocaleString() + ' 个文件 · ' + bytes(current.total_size) : '尚无状态记录') + ' <span class="divider">/</span> ' + directory.history_count + ' 个历史节点' + (!directory.available ? ' <span class="offline-label">目录离线</span>' : '') + '</small></span><span class="directory-date"><small>' + (directory.current ? '上次扫描' : '最新历史') + '</small>' + (current ? esc(date(current.created_time)) : '尚未记录') + '</span></button><div class="directory-actions"><button class="button secondary small" data-compare="' + esc(directory.id) + '">比较</button><button class="button secondary small" data-update="' + esc(directory.id) + '">更新</button></div></article>';
      }).join('') || '<div class="empty"><span class="empty-icon">▱</span><strong>先记录一个目录</strong><p>选择文件夹创建第一张快照，之后就可以检查变化、保存历史和比较时间节点。</p><button id="empty-create-btn" class="button primary">创建快照</button></div>';
      $('directory-list').querySelectorAll('[data-open]').forEach(button => button.onclick = () => action(button, () => openDirectory(button.dataset.open)));
      $('directory-list').querySelectorAll('[data-compare]').forEach(button => button.onclick = () => action(button, async () => {
        await openDirectory(button.dataset.compare); await SD.history.open();
      }));
      $('directory-list').querySelectorAll('[data-update]').forEach(button => button.onclick = () => startScan({ mode: 'capture', directoryId: button.dataset.update }));
      if ($('empty-create-btn')) $('empty-create-btn').onclick = () => openCreate();
      if (directoryReadFailed) { SD.notice('app-error', ''); directoryReadFailed = false; }
      syncBusy();
    } catch (error) { directoryReadFailed = true; SD.notice('app-error', '目录读取失败：' + error); throw error; }
  }

  async function openDirectory(id, autoCompare = true) {
    dropRequest++;
    const request = ++s.workspaceRequest;
    s.compareRequest++; s.queryRequest++; s.compareLoading = false;
    const [directory, history] = await Promise.all([invoke('get_directory', { directoryId: id }), invoke('list_history', { directoryId: id })]);
    if (request !== s.workspaceRequest) return;
    s.directory = directory; s.history = history; s.oldId = directory.baseline_id; s.target = 'current';
    SD.results.clear(); SD.results.resetFilters();
    SD.notice('update-notice', ''); SD.showView('workspace'); renderWorkspace();
    if (autoCompare) await compare();
  }
  async function refreshWorkspace(preserveSides = true) {
    if (!s.directory) return;
    s.compareRequest++; s.queryRequest++; s.compareLoading = false;
    const id = s.directory.id, oldId = s.oldId, target = s.target, request = ++s.workspaceRequest;
    const [directory, history] = await Promise.all([invoke('get_directory', { directoryId: id }), invoke('list_history', { directoryId: id })]);
    if (request !== s.workspaceRequest || s.directory?.id !== id) return;
    s.directory = directory; s.history = history;
    s.oldId = preserveSides && history.some(n => n.id === oldId) ? oldId : directory.baseline_id;
    s.target = preserveSides && (target === 'current' || history.some(n => n.id === target)) ? target : 'current';
    SD.results.clear(); renderWorkspace(); await compare();
  }
  function renderWorkspace() {
    if (!s.directory) return;
    const directory = s.directory;
    $('directory-name').textContent = directory.name; $('directory-path').textContent = directory.root_path;
    SD.notice('directory-warning', !directory.available ? '目录当前不可访问。仍可比较已保存的历史节点；检查变化和更新需要原目录可访问。' : '');
    renderSides();
    const stats = directory.current || directory.latest;
    $('stats-summary').textContent = stats ? stats.file_count.toLocaleString() + ' 个文件 / ' + stats.directory_count.toLocaleString() + ' 个目录 / ' + bytes(stats.total_size) : '';
    $('type-stats').innerHTML = Object.entries(directory.current_types || {}).sort((a, b) => b[1].size - a[1].size).map(([type, stat]) => '<div class="type-stat"><strong>' + esc(type) + '</strong><span>' + stat.count.toLocaleString() + ' 个</span><b>' + bytes(stat.size) + '</b></div>').join('') || '<p class="muted">暂无文件类型统计</p>';
    syncBusy();
  }
  function renderSides() {
    if (!s.directory) return;
    const old = s.history.find(n => n.id === s.oldId) || (s.comparison?.old.id === s.oldId ? s.comparison.old : null);
    $('baseline-time').textContent = old ? date(old.created_time) : '尚未选择历史基准';
    $('baseline-meta').textContent = old ? old.file_count.toLocaleString() + ' 个文件 · ' + bytes(old.total_size) + (old.remark ? ' · ' + old.remark : '') : '保存第一张快照后即可比较';
    $('target-select').innerHTML = '<option value="current">当前扫描状态' + (s.directory.current ? ' · ' + esc(date(s.directory.current.created_time)) : ' · 尚未扫描') + '</option>' + s.history.map(node => '<option value="' + esc(node.id) + '">' + esc(date(node.created_time)) + (node.id === s.history[0]?.id ? ' · 最新历史' : '') + (node.remark ? ' · ' + esc(node.remark) : '') + '</option>').join('');
    $('target-select').value = s.target;
    const next = s.target === 'current' ? s.directory.current : s.history.find(n => n.id === s.target);
    $('target-meta').textContent = next ? next.file_count.toLocaleString() + ' 个文件 · ' + bytes(next.total_size) : s.directory.available ? '点击“检查变化”读取当前目录' : '请选择一个历史节点作为对照';
    const reverse = old && next && new Date(old.created_time) > new Date(next.created_time);
    $('comparison-hint').textContent = (s.target === 'current' ? '当前状态是上次扫描结果，并非实时状态。检查变化不会新增历史节点。' : '历史节点之间的比较不读取目录文件。') + (reverse ? ' 当前为反向比较：基准时间晚于对照时间。' : '');
    syncBusy();
  }
  async function compare() {
    const directoryId = s.directory?.id;
    const request = ++s.compareRequest, oldId = s.oldId, target = s.target;
    if (!directoryId || !oldId || (target === 'current' && !s.directory.current)) {
      SD.results.clear();
      $('workspace-empty').textContent = !oldId ? '还没有历史基准。点击更新快照保存第一个时间节点。' : s.directory.available ? '还没有当前扫描状态。点击检查变化，或选择另一个历史节点作为对照。' : '该目录暂无当前扫描状态，请选择历史节点进行离线比较。';
      renderSides(); return;
    }
    if (oldId === target) {
      SD.results.clear(); $('workspace-empty').textContent = '基准和对照是同一个时间节点，请选择另一个节点。'; renderSides(); return;
    }
    s.compareLoading = true; syncBusy();
    SD.results.clear(); $('workspace-empty').textContent = '正在比较状态…';
    try {
      const result = await invoke('compare_states', { directoryId, oldId, newId: target === 'current' ? null : target });
      if (request !== s.compareRequest || s.directory?.id !== directoryId) return;
      s.query.page = 0; await SD.results.display(result);
    } catch (error) {
      if (request !== s.compareRequest) return;
      $('workspace-empty').textContent = String(error); toast(String(error), true);
    } finally {
      if (request === s.compareRequest) { s.compareLoading = false; syncBusy(); }
    }
  }
  function goHome() {
    dropRequest++;
    s.workspaceRequest++; s.compareRequest++; s.queryRequest++; s.compareLoading = false;
    s.directory = null; SD.results.clear(); SD.showView('home');
    action(null, refreshDirectories);
  }
  function openCreate(rootPath = '') {
    if (s.task) return;
    dropRequest++;
    $('root-path').value = rootPath; $('ignore-patterns').value = '';
    SD.open('create-modal'); $('root-path').focus();
  }
  function canAcceptDrop() {
    return $('startup-screen').hidden && $('home-view').classList.contains('active')
      && !s.task && !document.querySelector('dialog[open]');
  }
  async function onDirectoryDrop(event) {
    const request = ++dropRequest;
    if (!canAcceptDrop()) return;
    const paths = event.payload?.paths;
    if (!Array.isArray(paths) || paths.length !== 1 || typeof paths[0] !== 'string') {
      toast('每次请只拖入一个文件夹', true); return;
    }
    try {
      const root = await invoke('validate_directory_path', { path: paths[0] });
      // Validation may complete after another drop, navigation or dialog opening.
      if (request === dropRequest && canAcceptDrop()) openCreate(root);
    } catch (error) {
      if (request === dropRequest && canAcceptDrop()) toast(String(error), true);
    }
  }
  function newTaskId() {
    if (crypto.randomUUID) return crypto.randomUUID();
    return '10000000-1000-4000-8000-100000000000'.replace(/[018]/g, c => (Number(c) ^ crypto.getRandomValues(new Uint8Array(1))[0] & 15 >> Number(c) / 4).toString(16));
  }
  async function startScan(options) {
    if (s.task) return;
    const taskId = newTaskId();
    const request = { taskId, mode: options.mode, directoryId: options.directoryId || null, rootPath: options.rootPath || null, ignorePatterns: options.ignorePatterns || [], remark: '' };
    s.task = { id: taskId, mode: request.mode, cancelling: false, saving: false };
    s.lastScanRequest = { ...options };
    SD.close('create-modal'); SD.close('history-modal'); SD.close('settings-modal');
    $('scan-panel').classList.remove('hidden', 'scan-error');
    $('scan-title').textContent = request.mode === 'check' ? '检查目录变化' : request.mode === 'create' ? '创建第一个时间节点' : '更新目录快照';
    $('progress-text').textContent = '正在准备…'; $('progress-percent').textContent = '';
    $('current-file').textContent = options.rootPath || s.directories.find(d => d.id === options.directoryId)?.root_path || s.directory?.root_path || '';
    $('progress-bar').style.width = '0%'; $('progress-bar').classList.add('indeterminate');
    $('cancel-scan-btn').classList.remove('hidden'); $('cancel-scan-btn').disabled = false; $('cancel-scan-btn').textContent = '取消扫描';
    $('retry-scan-btn').classList.add('hidden'); $('dismiss-scan-btn').classList.add('hidden');
    syncBusy();
    let scanCompleted = false;
    try {
      const result = await invoke('scan_directory', { request });
      if (s.task?.id !== taskId) return;
      scanCompleted = true;
      s.task = null; $('scan-panel').classList.add('hidden'); syncBusy();
      await refreshDirectories();
      if (request.mode === 'create' || s.directory?.id === result.directory_id) {
        await openDirectory(result.directory_id, false);
        if (result.comparison) {
          await SD.results.display(result.comparison);
          if (result.saved) SD.notice('update-notice', '已保存新时间节点，默认基准已更新。下方保留本次更新前后的差异。' + (result.pruned ? ' 已按保留规则清理 ' + result.pruned + ' 个旧节点。' : ''));
        } else { await compare(); }
      }
      toast(result.saved ? '新时间节点已保存' : '检查完成，未新增历史节点');
    } catch (error) {
      if (s.task?.id === taskId) s.task = null;
      if (scanCompleted) {
        $('scan-panel').classList.add('hidden');
        toast('操作已完成，但界面刷新失败，请刷新目录列表：' + error, true);
        syncBusy(); return;
      }
      $('progress-bar').classList.remove('indeterminate');
      $('cancel-scan-btn').classList.add('hidden'); $('dismiss-scan-btn').classList.remove('hidden');
      if (String(error).includes('扫描已取消')) {
        $('scan-panel').classList.add('hidden'); toast('扫描已取消，本次状态未保存');
      } else {
        $('scan-panel').classList.add('scan-error');
        $('scan-title').textContent = '本次操作未完成';
        $('progress-text').textContent = String(error);
        $('progress-percent').textContent = '';
        $('retry-scan-btn').classList.remove('hidden');
        toast(String(error), true);
      }
      syncBusy();
    }
  }
  function onProgress(event) {
    const p = event.payload;
    if (!s.task || p.task_id !== s.task.id) return;
    s.task.saving = ['saving', 'completed'].includes(p.phase);
    if (s.task.saving) { $('cancel-scan-btn').disabled = true; $('cancel-scan-btn').textContent = '正在完成…'; }
    const hashingBytes = p.phase === 'hashing' ? p.current_file_bytes : 0;
    const fraction = p.current_file_size ? hashingBytes / p.current_file_size : 0;
    const percent = p.total_bytes ? (p.processed_bytes + hashingBytes) * 100 / p.total_bytes : p.total ? (p.processed + fraction) * 100 / p.total : 0;
    const indeterminate = ['collecting', 'comparing', 'saving'].includes(p.phase);
    $('progress-bar').classList.toggle('indeterminate', indeterminate);
    $('progress-bar').style.width = Math.min(100, percent).toFixed(1) + '%';
    $('progress-percent').textContent = !indeterminate && p.total ? Math.min(100, percent).toFixed(1) + '%' : '';
    const phases = { collecting: '正在读取目录结构…', comparing: '正在计算差异…', saving: '正在提交完整状态…', completed: '已完成' };
    $('progress-text').textContent = s.task.cancelling ? '正在取消，等待当前读取返回…' : phases[p.phase] || '已处理 ' + p.processed.toLocaleString() + ' / ' + p.total.toLocaleString() + ' 项';
    $('current-file').textContent = (p.current_path || '') + (p.phase === 'hashing' ? ' · MD5 ' + bytes(p.current_file_bytes) + ' / ' + bytes(p.current_file_size) : '');
  }
  function syncBusy() {
    const busy = !!s.task;
    const toggle = (id, disabled) => {
      const button = $(id);
      if (button) button.disabled = !!disabled || button.dataset.working === 'true';
    };
    toggle('new-snapshot-btn', busy); toggle('empty-create-btn', busy); toggle('import-btn', busy);
    toggle('check-btn', busy || !s.directory?.available); toggle('capture-btn', busy || !s.directory?.available);
    toggle('save-settings-btn', busy); toggle('delete-directory-btn', busy);
    const validTarget = s.target === 'current' ? !!s.directory?.current : s.history.some(n => n.id === s.target);
    toggle('compare-btn', s.compareLoading || !s.oldId || !s.history.some(n => n.id === s.oldId) || !validTarget || s.oldId === s.target);
    toggle('target-select', s.compareLoading);
    toggle('select-all-nodes', busy);
    toggle('cancel-scan-btn', s.task?.saving || s.task?.cancelling);
    $('create-btn').classList.toggle('hidden', busy); toggle('create-btn', busy);
    $('root-path').disabled = $('ignore-patterns').disabled = busy; toggle('browse-btn', busy);
    document.querySelectorAll('[data-update], [data-baseline], [data-target], [data-remark], [data-focus]').forEach(button => button.disabled = busy || button.dataset.working === 'true' || (button.dataset.target && button.dataset.target === s.oldId) || (button.dataset.update && !s.directories.find(d => d.id === button.dataset.update)?.available));
    if (busy) { toggle('delete-nodes-btn', true); toggle('export-nodes-btn', true); }
  }
  function openSettings() {
    if (!s.directory) return;
    $('settings-ignore').value = s.directory.ignore_patterns.join('\n');
    $('settings-focus').value = s.directory.focus_patterns.join('\n');
    $('retention-mode').value = !s.directory.retention ? 'all' : s.directory.retention === 1 ? 'latest' : 'custom';
    $('retention-number').value = s.directory.retention || 10;
    $('retention-number-field').classList.toggle('hidden', $('retention-mode').value !== 'custom');
    SD.open('settings-modal'); syncBusy();
  }
  async function saveSettings() {
    const directoryId = s.directory.id;
    const mode = $('retention-mode').value;
    const retention = mode === 'all' ? 0 : mode === 'latest' ? 1 : Number($('retention-number').value);
    if (!Number.isInteger(retention) || retention < 0 || retention > 100000 || (mode === 'custom' && !retention)) throw new Error('请输入 1 至 100000 的保留数量');
    const nodes = await invoke('preview_retention', { directoryId, retention });
    if (nodes.length && !await SD.confirm('清理较早的历史节点', '保存设置后将立即删除以下 ' + nodes.length + ' 个快照记录。', nodes.map(n => date(n.created_time) + (n.remark ? ' · ' + n.remark : '')).join('\n'))) return;
    const pruned = await invoke('update_settings', { directoryId, ignorePatterns: SD.lines($('settings-ignore').value), focusPatterns: SD.lines($('settings-focus').value), retention });
    SD.close('settings-modal'); SD.notice('update-notice', '');
    await refreshWorkspace(true); await refreshDirectories(); toast('设置已保存' + (pruned ? '，已清理 ' + pruned + ' 个历史节点' : ''));
  }
  async function exportData(directoryId = null, snapshotIds = []) {
    const directory = s.directories.find(d => d.id === directoryId) || (s.directory?.id === directoryId ? s.directory : null);
    const path = await SD.chooseSave((directory ? directory.name : 'SnapDiff') + '_快照数据.json', 'json');
    if (!path) return;
    await invoke('export_snapshots', { directoryId, snapshotIds, path }); toast('快照数据已导出');
  }
  async function importData() {
    const path = await SD.chooseOpen(); if (!path) return;
    const ticket = await invoke('preview_import', { path });
    const preview = ticket.preview;
    if (!await SD.confirm('导入快照数据', preview.directories + ' 个目录 · ' + preview.snapshots + ' 个节点\n新增 ' + preview.new_snapshots + ' 个，跳过重复 ' + preview.duplicates + ' 个。', preview.roots.join('\n'))) return;
    const result = await invoke('import_snapshots', { ticketId: ticket.ticket_id });
    await refreshDirectories();
    if (s.directory) await refreshWorkspace(true);
    toast('已导入 ' + result.new_snapshots + ' 个历史节点');
  }
  function bind() {
    $('brand-home').onclick = $('back-btn').onclick = goHome;
    $('new-snapshot-btn').onclick = () => openCreate();
    $('refresh-btn').onclick = () => action($('refresh-btn'), refreshDirectories);
    $('browse-btn').onclick = () => action($('browse-btn'), async () => { const path = await SD.chooseDirectory(); if (path) $('root-path').value = path; });
    $('create-form').onsubmit = event => {
      event.preventDefault();
      const root = $('root-path').value.trim();
      if (!root) { toast('请选择目录', true); return; }
      startScan({ mode: 'create', rootPath: root, ignorePatterns: SD.lines($('ignore-patterns').value) });
    };
    $('check-btn').onclick = () => startScan({ mode: 'check', directoryId: s.directory.id });
    $('capture-btn').onclick = () => startScan({ mode: 'capture', directoryId: s.directory.id });
    $('cancel-scan-btn').onclick = () => action($('cancel-scan-btn'), async () => {
      if (!s.task || s.task.saving) return;
      const task = s.task;
      task.cancelling = true; $('cancel-scan-btn').textContent = '正在取消…';
      $('progress-text').textContent = '正在取消，等待当前读取返回…';
      try { await invoke('cancel_scan', { taskId: task.id }); }
      catch (error) {
        if (s.task?.id === task.id) { task.cancelling = false; toast(String(error), true); }
      }
      if (s.task?.id === task.id) $('cancel-scan-btn').disabled = true;
    });
    $('retry-scan-btn').onclick = () => startScan(s.lastScanRequest);
    $('dismiss-scan-btn').onclick = () => $('scan-panel').classList.add('hidden');
    $('baseline-btn').onclick = () => action($('baseline-btn'), SD.history.open);
    $('target-select').onchange = () => { s.target = $('target-select').value; SD.notice('update-notice', ''); compare(); };
    $('compare-btn').onclick = () => action($('compare-btn'), compare);
    $('settings-btn').onclick = openSettings;
    $('retention-mode').onchange = () => $('retention-number-field').classList.toggle('hidden', $('retention-mode').value !== 'custom');
    $('settings-form').onsubmit = event => { event.preventDefault(); action($('save-settings-btn'), saveSettings); };
    $('export-directory-btn').onclick = () => action($('export-directory-btn'), () => exportData(s.directory.id));
    $('export-all-btn').onclick = () => action($('export-all-btn'), () => exportData());
    $('import-btn').onclick = () => action($('import-btn'), importData);
    $('delete-directory-btn').onclick = () => action($('delete-directory-btn'), async () => {
      const directory = s.directory;
      if (!await SD.confirm('移除目录记录', '将删除“' + directory.name + '”的目录记录及全部历史快照。目录中的实际文件不受影响。')) return;
      await invoke('delete_directory', { directoryId: directory.id });
      SD.close('settings-modal'); SD.close('history-modal'); goHome(); toast('目录记录已移除');
    });
  }
  function init() {
    if (initialized) return readyPromise;
    initialized = true;
    readyPromise = (async () => {
      try {
        SD.bindCommon(); SD.results.bind(); SD.history.bind(); bind();
        if (window.__TAURI__?.event?.listen) {
          await window.__TAURI__.event.listen('scan-progress', onProgress);
          await window.__TAURI__.event.listen('tauri://drag-drop', onDirectoryDrop);
        }
        await refreshDirectories();
        const info = await invoke('get_storage_info');
        if (info?.warnings?.length) SD.notice('app-error', '以下旧快照未迁移，原文件保留：\n' + info.warnings.join('\n'));
      } catch (error) {
        if (!$('app-error').textContent) SD.notice('app-error', '启动未完成，请刷新目录列表重试：' + error);
        toast(String(error), true);
      } finally {
        // Errors must also exit loading so the user can see and retry them.
        $('startup-screen').hidden = true;
        document.querySelector('.app-shell').removeAttribute('inert');
        document.querySelector('.app-shell').setAttribute('aria-busy', 'false');
      }
    })();
    return readyPromise;
  }
  return { init, syncBusy, refreshDirectories, openDirectory, refreshWorkspace, renderSides, compare, startScan, openSettings, exportData, goHome };
})();
document.addEventListener('DOMContentLoaded', () => SD.app.init(), { once: true });
