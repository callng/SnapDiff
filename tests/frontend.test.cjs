const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { JSDOM } = require('jsdom');

const root = path.join(__dirname, '..');
const clone = value => JSON.parse(JSON.stringify(value));
const turn = () => new Promise(resolve => setImmediate(resolve));
const summary = (id, time = '2026-10-12T01:00:00Z') => ({
  id, directory_id: 'dir', created_time: time, scan_started_time: time, file_count: 4,
  directory_count: 1, total_size: 1024, hash_algorithm: 'blake3', remark: '', directory_entries_complete: true,
});
const older = summary('older', '2026-10-11T01:00:00Z'), latest = summary('latest');
const entry = relative_path => ({ relative_path, kind: 'file', size: 1024, modified_time: latest.created_time, hash: 'a'.repeat(64) });

async function app({ current = true, rowCount = 1, available = true, startupDirectoryRead, validateDroppedPath, migrateDatabase, compareStates, algorithm = 'blake3' } = {}) {
  const dom = new JSDOM(fs.readFileSync(path.join(root, 'frontend/index.html'), 'utf8'), { runScripts: 'outside-only', url: 'http://localhost' });
  const { window } = dom;
  await new Promise(resolve => window.document.readyState === 'loading' ? window.document.addEventListener('DOMContentLoaded', resolve, { once: true }) : resolve());
  window.HTMLDialogElement.prototype.showModal = function() { this.open = true; };
  window.HTMLDialogElement.prototype.close = function() { this.open = false; this.dispatchEvent(new window.Event('close')); };
  const directory = {
    id: 'dir', name: 'Folder', root_path: 'C:/folder', root_key: 'c:/folder',
    baseline_id: 'latest', current_id: current ? 'latest' : null, ignore_patterns: [], focus_patterns: [],
    retention: 0, history_count: 2, latest: clone(latest), current: current ? clone(latest) : null,
    current_types: { '.bin': { count: 4, size: 1024 } }, available,
  };
  let directories = [directory], nodes = [clone(latest), clone(older)], pending;
  for (const node of [directory.latest, directory.current, ...nodes]) if (node) node.hash_algorithm = algorithm;
  const calls = [], listeners = {}, rows = Array.from({ length: rowCount }, (_, n) => ({
    path: n === 0 ? 'mods/<img onerror="bad">.bin' : 'mods/' + n + '.bin',
    status: 'added', old: null, new: { ...entry(n === 0 ? 'mods/<img onerror="bad">.bin' : 'mods/' + n + '.bin'), hash: 'a'.repeat(algorithm === 'md5' ? 32 : 64) }, size_delta: 1024, focused: false,
  }));
  const comparison = (old, next, targetCurrent) => ({
    id: 'comparison-' + old.id + '-' + next.id, directory_id: 'dir', old: clone(old), new: clone(next), target_current: targetCurrent,
    counts: { added: old.id === next.id ? 0 : rowCount, deleted: 0, modified: 0, moved: 0, metadata: 0 },
    size_delta: 0, scope_changed: false, excluded_scope_count: 0, directory_structure_available: true,
  });
  let lastComparison;
  window.navigator.clipboard = { writeText: async text => { window.copiedText = text; } };
  window.__TAURI__ = {
    core: { invoke: async (cmd, args = {}) => {
      calls.push({ cmd, args: clone(args) });
      switch (cmd) {
        case 'list_directories':
          if (startupDirectoryRead) await startupDirectoryRead({ window, calls });
          return clone(directories);
        case 'get_directory': return clone(directories.find(d => d.id === args.directoryId));
        case 'get_storage_info': return { warnings: [], mode: 'default', database_path: 'C:/appdata/snapdiff.sqlite3', directory: 'C:/appdata', default_directory: 'C:/appdata', install_directory: 'C:/SnapDiff/data' };
        case 'migrate_database':
          if (migrateDatabase) return migrateDatabase(args.directory);
          return { warnings: [], mode: 'custom', database_path: args.directory + '/snapdiff.sqlite3', directory: args.directory, default_directory: 'C:/appdata', install_directory: 'C:/SnapDiff/data', source_retained: false };
        case 'validate_directory_path':
          if (validateDroppedPath) return validateDroppedPath(args.path);
          return args.path;
        case 'list_history': return clone(nodes);
        case 'get_activity': return [{ date: '2026-10-12', scans: 2, captures: 1, changes: 3 }];
        case 'select_baseline': directory.baseline_id = args.snapshotId; return;
        case 'scan_directory': return new Promise((resolve, reject) => { pending = { resolve, reject, request: args.request }; });
        case 'cancel_scan': pending.reject('扫描已取消'); return;
        case 'compare_states': {
          if (compareStates) return compareStates(args);
          const old = nodes.find(n => n.id === args.oldId);
          const next = args.newId ? nodes.find(n => n.id === args.newId) : directory.current;
          lastComparison = comparison(old, next, !args.newId); return lastComparison;
        }
        case 'query_diff': {
          const selected = lastComparison?.counts.added ? rows : [];
          const page = args.query.page, start = page * 100;
          return { rows: clone(selected.slice(start, start + 100)), folders: [], total: selected.length, page, page_size: 100 };
        }
        case 'delete_nodes':
          nodes = nodes.filter(n => !args.snapshotIds.includes(n.id));
          directory.history_count = nodes.length; directory.latest = nodes[0] || null;
          if (args.snapshotIds.includes(directory.baseline_id)) directory.baseline_id = nodes[0]?.id || null;
          if (args.snapshotIds.includes(directory.current_id)) { directory.current_id = null; directory.current = null; }
          return;
        case 'preview_retention': return clone(args.retention ? nodes.slice(args.retention) : []);
        case 'update_settings':
          directory.ignore_patterns = args.ignorePatterns; directory.focus_patterns = args.focusPatterns; directory.retention = args.retention;
          return 0;
        case 'set_focus_patterns':
          directory.focus_patterns = args.focusPatterns;
          rows[0].focused = true;
          return;
        case 'get_diff_paths': return rows.map(r => r.path).join('\n');
        case 'get_report': return { text: '差异报告', bytes: 12, truncated: false };
        case 'preview_import': return { ticket_id: 'immutable-ticket', preview: { directories: 1, snapshots: 2, new_snapshots: 1, duplicates: 1, roots: ['C:/folder'] } };
        case 'import_snapshots': return { new_snapshots: 1, duplicates: 1 };
        case 'delete_directory': directories = []; return;
        default: return;
      }
    } },
    event: { listen: async (name, handler) => { listeners[name] = handler; return () => {}; } },
    dialog: { open: async () => 'C:/data.json', save: async () => 'C:/export.json' },
  };
  for (const file of ['core.js', 'results.js', 'history.js', 'app.js']) window.eval(fs.readFileSync(path.join(root, 'frontend', file), 'utf8'));
  await window.SD.app.init();
  const result = {
    window, calls, listeners, el: id => window.document.getElementById(id), close: () => window.close(),
    get pending() { return pending; },
    async completeScan({ notice = null } = {}) {
      const request = pending.request, saved = request.mode !== 'check', newNode = summary('scanned', '2026-10-12T02:00:00Z');
      const old = clone(latest);
      directory.current = clone(newNode); directory.current_id = newNode.id;
      if (saved) { nodes.unshift(newNode); directory.baseline_id = newNode.id; directory.latest = newNode; directory.history_count++; }
      lastComparison = notice ? null : comparison(old, newNode, !saved);
      pending.resolve({ directory_id: directory.id, state: clone(newNode), comparison: lastComparison, notice, saved, pruned: 0 });
      await turn(); await turn(); await turn();
    },
  };
  return result;
}

test('database position selection is explicit and cancelling migration does not change records', async () => {
  const a = await app();
  try {
    a.el('storage-btn').click(); await turn();
    assert(a.el('storage-modal').open);
    assert.equal(a.el('storage-current').textContent, 'C:/appdata/snapdiff.sqlite3');
    assert.equal(a.calls.filter(c => c.cmd === 'scan_directory' || c.cmd === 'migrate_database').length, 0);
    a.el('storage-mode').value = 'install'; a.el('storage-mode').dispatchEvent(new a.window.Event('change'));
    assert.equal(a.el('storage-directory').value, 'C:/SnapDiff/data');
    assert(a.el('storage-browse').disabled);
    a.el('storage-form').dispatchEvent(new a.window.Event('submit', { cancelable: true })); await turn();
    assert(a.el('confirm-modal').open);
    assert.match(a.el('confirm-details').textContent, /C:\/SnapDiff\/data/);
    a.el('confirm-no').click(); await turn();
    assert.equal(a.calls.filter(c => c.cmd === 'migrate_database').length, 0);
    assert(a.el('storage-modal').open);
  } finally { a.close(); }
});

test('database migration blocks competing operations, survives failure and accepts a retry', async () => {
  let resolveMigration, rejectMigration;
  const a = await app({ migrateDatabase: () => new Promise((resolve, reject) => { resolveMigration = resolve; rejectMigration = reject; }) });
  try {
    a.el('storage-btn').click(); await turn();
    a.el('storage-mode').value = 'custom'; a.el('storage-mode').dispatchEvent(new a.window.Event('change'));
    a.window.__TAURI__.dialog.open = async () => 'D:/数据库 <folder>';
    a.el('storage-browse').click(); await turn();
    a.el('storage-form').dispatchEvent(new a.window.Event('submit', { cancelable: true })); await turn();
    a.el('confirm-yes').click(); await turn();
    assert.equal(a.calls.find(c => c.cmd === 'migrate_database').args.directory, 'D:/数据库 <folder>');
    for (const id of ['new-snapshot-btn', 'import-btn', 'storage-close', 'storage-cancel', 'storage-mode', 'storage-browse']) assert(a.el(id).disabled, id);
    assert.equal(a.el('storage-modal').dispatchEvent(new a.window.Event('cancel', { cancelable: true })), false);
    await a.window.SD.app.startScan({ mode: 'capture', directoryId: 'dir' });
    assert.equal(a.calls.filter(c => c.cmd === 'scan_directory').length, 0);
    rejectMigration('目标数据库已存在'); await turn(); await turn();
    assert.equal(a.el('storage-current').textContent, 'C:/appdata/snapdiff.sqlite3');
    assert.match(a.el('storage-status').textContent, /迁移失败.*原数据库/);
    assert.equal(a.el('new-snapshot-btn').disabled, false);
    assert.equal(a.el('storage-migrate').disabled, false);
    a.el('storage-form').dispatchEvent(new a.window.Event('submit', { cancelable: true })); await turn();
    a.el('confirm-yes').click(); await turn();
    resolveMigration({ warnings: [], directory: 'D:/数据库 <folder>', database_path: 'D:/数据库 <folder>/snapdiff.sqlite3', source_retained: false });
    await turn(); await turn();
    assert.equal(a.el('storage-current').textContent, 'D:/数据库 <folder>/snapdiff.sqlite3');
    assert.equal(a.el('storage-current').children.length, 0);
    assert.match(a.el('storage-status').textContent, /下次启动/);
    assert.equal(a.el('storage-close').disabled, false);
    assert.equal(a.calls.filter(c => c.cmd === 'scan_directory').length, 0);
  } finally { a.close(); }
});

test('a running scan disables database relocation and leaves scan cancellation available', async () => {
  const a = await app();
  try {
    const scanning = a.window.SD.app.startScan({ mode: 'capture', directoryId: 'dir' }); await turn();
    assert(a.el('storage-btn').disabled);
    assert.equal(a.el('cancel-scan-btn').disabled, false);
    a.el('storage-btn').click(); await turn(); assert(!a.el('storage-modal').open);
    a.el('cancel-scan-btn').click(); await scanning;
    assert.equal(a.el('storage-btn').disabled, false);
  } finally { a.close(); }
});

test('default context menus and browser file navigation are blocked throughout the page', async () => {
  const a = await app();
  try {
    a.el('new-snapshot-btn').click();
    for (const target of [a.window.document.body, a.el('root-path'), a.el('ignore-patterns')]) {
      const menu = new a.window.MouseEvent('contextmenu', { bubbles: true, cancelable: true, button: 2 });
      assert.equal(target.dispatchEvent(menu), false);
      assert(menu.defaultPrevented);
    }
    for (const type of ['dragover', 'drop']) {
      const drop = new a.window.Event(type, { bubbles: true, cancelable: true });
      assert.equal(a.el('home-view').dispatchEvent(drop), false);
    }
  } finally { a.close(); }
});

test('dropping a folder opens a prefilled form without scanning until the user submits', async () => {
  const a = await app();
  try {
    const path = 'D:\\工作目录\\项目 & <test>';
    await a.listeners['tauri://drag-drop']({ payload: { paths: [path] } });
    assert(a.el('create-modal').open);
    assert.equal(a.el('root-path').value, path);
    assert.equal(a.el('ignore-patterns').value, '');
    assert.equal(a.calls.filter(c => c.cmd === 'scan_directory').length, 0);
    a.el('ignore-patterns').value = '*.log\ncache/';
    a.el('create-form').dispatchEvent(new a.window.Event('submit', { cancelable: true }));
    await turn();
    assert.equal(a.pending.request.rootPath, path);
    assert.deepEqual(clone(a.pending.request.ignorePatterns), ['*.log', 'cache/']);
    assert(!a.el('create-modal').open);
    a.el('cancel-scan-btn').click(); await turn(); await turn();
  } finally { a.close(); }
});

test('dropping files or managed folders reports the validation error without opening a form', async () => {
  for (const message of ['请拖入一个可访问的文件夹，不能拖入文件', '该目录已经存在，请使用对应目录的更新按钮']) {
    const a = await app({ validateDroppedPath: async () => { throw new Error(message); } });
    try {
      await a.listeners['tauri://drag-drop']({ payload: { paths: ['C:/invalid'] } });
      assert(!a.el('create-modal').open);
      assert(a.el('toast').textContent.includes(message));
      assert.equal(a.calls.filter(c => c.cmd === 'scan_directory').length, 0);
    } finally { a.close(); }
  }
});

test('drops reject multiple paths and preserve open dialogs, scans and workspaces', async () => {
  const a = await app();
  try {
    const drop = paths => a.listeners['tauri://drag-drop']({ payload: { paths } });
    await drop(['C:/one', 'C:/two']);
    assert(!a.el('create-modal').open);
    assert.match(a.el('toast').textContent, /只拖入一个文件夹/);
    a.el('new-snapshot-btn').click();
    a.el('root-path').value = 'C:/manual'; a.el('ignore-patterns').value = '*.log';
    await drop(['C:/other']);
    assert.equal(a.el('root-path').value, 'C:/manual');
    assert.equal(a.el('ignore-patterns').value, '*.log');
    a.el('create-modal').close();
    a.window.SD.state.task = { id: 'busy' };
    await drop(['C:/other']);
    assert(!a.el('create-modal').open);
    a.window.SD.state.task = null;
    await a.window.SD.app.openDirectory('dir');
    await drop(['C:/other']);
    assert(!a.el('create-modal').open);
    assert.equal(a.calls.filter(c => c.cmd === 'validate_directory_path').length, 0);
  } finally { a.close(); }
});

test('late drop validation cannot overwrite manual configuration or a newer drop', async () => {
  const pending = [];
  const a = await app({ validateDroppedPath: path => new Promise(resolve => pending.push(() => resolve(path))) });
  try {
    const drop = path => a.listeners['tauri://drag-drop']({ payload: { paths: [path] } });
    const first = drop('C:/first');
    a.el('new-snapshot-btn').click(); a.el('ignore-patterns').value = 'keep-this-rule';
    pending.shift()(); await first;
    assert.equal(a.el('root-path').value, '');
    assert.equal(a.el('ignore-patterns').value, 'keep-this-rule');
    a.el('create-modal').close();
    const older = drop('C:/older'), newer = drop('C:/newer');
    pending.pop()(); await newer;
    pending.shift()(); await older;
    assert.equal(a.el('root-path').value, 'C:/newer');
    assert.equal(a.calls.filter(c => c.cmd === 'scan_directory').length, 0);
  } finally { a.close(); }
});

test('startup stays in loading while data is pending and unlocks the homepage when ready', async () => {
  let observed = false;
  const a = await app({ startupDirectoryRead: async ({ window }) => {
    await turn();
    observed = true;
    assert.equal(window.document.getElementById('startup-screen').hidden, false);
    assert(window.document.querySelector('.app-shell').hasAttribute('inert'));
    assert.equal(window.document.querySelector('.app-shell').getAttribute('aria-busy'), 'true');
  } });
  try {
    assert(observed);
    assert(a.el('startup-screen').hidden);
    assert(!a.window.document.querySelector('.app-shell').hasAttribute('inert'));
    assert.match(a.el('directory-list').textContent, /Folder/);
  } finally { a.close(); }
});

test('failed startup reveals its error and the directory list can be retried', async () => {
  let fail = true;
  const a = await app({ startupDirectoryRead: async () => { if (fail) throw new Error('数据库暂不可用'); } });
  try {
    assert(a.el('startup-screen').hidden);
    assert(!a.window.document.querySelector('.app-shell').hasAttribute('inert'));
    assert.match(a.el('app-error').textContent, /数据库暂不可用/);
    fail = false;
    a.el('refresh-btn').click(); await turn(); await turn();
    assert.match(a.el('directory-list').textContent, /Folder/);
  } finally { a.close(); }
});

test('manual check uses a task ID, locks creation, ignores stale progress, and cancellation discards the operation', async () => {
  const a = await app();
  try {
    await a.window.SD.app.openDirectory('dir');
    a.el('check-btn').click(); await turn();
    const scan = a.calls.find(c => c.cmd === 'scan_directory');
    assert.equal(scan.args.request.mode, 'check');
    assert.match(scan.args.request.taskId, /^[0-9a-f-]{36}$/);
    assert(a.el('create-btn').classList.contains('hidden'));
    assert(a.el('capture-btn').disabled);
    a.el('check-btn').click();
    assert.equal(a.calls.filter(c => c.cmd === 'scan_directory').length, 1);
    const progress = { phase: 'hashing', processed: 0, total: 1, current_path: 'large.bin', current_file_bytes: 536870912, current_file_size: 1073741824, processed_bytes: 0, total_bytes: 1073741824 };
    a.listeners['scan-progress']({ payload: { ...progress, task_id: 'stale' } });
    assert.equal(a.el('progress-percent').textContent, '');
    a.listeners['scan-progress']({ payload: { ...progress, task_id: scan.args.request.taskId } });
    assert.equal(a.el('current-file').textContent, 'large.bin');
    assert.match(a.el('file-progress-size').textContent, /512.*1.00 GiB/);
    assert.equal(a.el('progress-percent').textContent, '50.0%');
    assert.equal(a.el('file-progress-percent').textContent, '50.0%');
    a.el('cancel-scan-btn').click(); await turn(); await turn();
    assert.equal(a.calls.find(c => c.cmd === 'cancel_scan').args.taskId, scan.args.request.taskId);
    assert(a.el('scan-panel').classList.contains('hidden'));
    assert(!a.el('capture-btn').disabled);
  } finally { a.close(); }
});

test('total and file progress stay independent across files and scan phases', async () => {
  const a = await app();
  try {
    a.window.SD.app.startScan({ mode: 'check', directoryId: 'dir' }); await turn();
    const task_id = a.pending.request.taskId;
    const send = p => a.listeners['scan-progress']({ payload: { task_id, ...p } });
    assert(a.el('progress-bar').classList.contains('indeterminate'));
    assert(a.el('file-progress').classList.contains('hidden'));
    const progress = { phase: 'hashing', processed: 3, total: 10, current_path: 'mods/<img onerror=bad>.bin', current_file_bytes: 50, current_file_size: 100, processed_bytes: 200, total_bytes: 1000 };
    send(progress);
    assert.equal(a.el('progress-bar').style.width, '25%');
    assert.equal(a.el('file-progress-bar').style.width, '50%');
    assert.equal(a.el('progress-size').textContent, '250 B / 1000 B');
    assert.equal(a.el('file-progress-size').textContent, '50 B / 100 B');
    assert.equal(a.el('total-progress-track').getAttribute('aria-valuenow'), '25.0');
    assert.equal(a.el('file-progress-track').getAttribute('aria-valuenow'), '50.0');
    assert(!a.el('file-progress').classList.contains('hidden'));
    assert.equal(a.el('current-file').textContent, progress.current_path);
    assert.equal(a.el('current-file').querySelector('img'), null);
    send({ ...progress, phase: 'scanning', processed: 4, current_file_bytes: 100, processed_bytes: 300 });
    assert.equal(a.el('progress-percent').textContent, '30.0%');
    assert.equal(a.el('file-progress-percent').textContent, '100.0%');
    send({ ...progress, processed: 4, current_path: 'next.bin', current_file_bytes: 0, processed_bytes: 300 });
    assert.equal(a.el('progress-percent').textContent, '30.0%');
    assert.equal(a.el('file-progress-percent').textContent, '0.0%');
    for (const phase of ['collecting', 'comparing', 'saving']) {
      send({ phase, processed: 0, total: 0, current_path: '', current_file_bytes: 0, current_file_size: 0, processed_bytes: 0, total_bytes: 0 });
      assert(a.el('progress-bar').classList.contains('indeterminate'), phase);
      assert(a.el('file-progress').classList.contains('hidden'), phase);
      assert.equal(a.el('progress-percent').textContent, '');
      assert.equal(a.el('progress-size').textContent, '');
      assert.equal(a.el('total-progress-track').hasAttribute('aria-valuenow'), false);
    }
    assert(a.el('cancel-scan-btn').disabled);
    await a.completeScan();
    assert(a.el('scan-panel').classList.contains('hidden'));
  } finally { a.close(); }
});

test('progress handles zero-byte entries and resets after a failed scan', async () => {
  const a = await app();
  try {
    a.window.SD.app.startScan({ mode: 'create', rootPath: 'C:/empty' }); await turn();
    const task_id = a.pending.request.taskId;
    const progress = { task_id, phase: 'hashing', processed: 1, total: 4, current_path: 'empty.txt', current_file_bytes: 0, current_file_size: 0, processed_bytes: 0, total_bytes: 0 };
    a.listeners['scan-progress']({ payload: progress });
    assert.equal(a.el('progress-percent').textContent, '25.0%');
    assert.equal(a.el('file-progress-percent').textContent, '100.0%');
    assert.equal(a.el('file-progress-size').textContent, '0 B / 0 B');
    a.listeners['scan-progress']({ payload: { ...progress, phase: 'scanning', processed: 2, current_path: 'empty-folder' } });
    assert.equal(a.el('progress-percent').textContent, '50.0%');
    assert(a.el('file-progress').classList.contains('hidden'));
    a.listeners['scan-progress']({ payload: { ...progress, phase: 'hashing', current_file_size: 100, current_file_bytes: 70, total_bytes: 100 } });
    a.pending.reject('文件读取失败'); await turn(); await turn();
    assert(a.el('scan-panel').classList.contains('scan-error'));
    assert(a.el('file-progress').classList.contains('hidden'));
    a.el('retry-scan-btn').click(); await turn();
    assert(a.el('progress-bar').classList.contains('indeterminate'));
    assert.equal(a.el('progress-percent').textContent, '');
    assert.equal(a.el('progress-size').textContent, '');
    assert.equal(a.el('file-progress-bar').style.width, '0%');
    assert(a.el('file-progress').classList.contains('hidden'));
    a.el('cancel-scan-btn').click(); await turn(); await turn();
    assert(a.el('scan-panel').classList.contains('hidden'));
  } finally { a.close(); }
});

test('checking changes preserves historical baseline and does not add a history node', async () => {
  const a = await app();
  try {
    await a.window.SD.app.openDirectory('dir');
    a.el('check-btn').click(); await turn(); await a.completeScan();
    assert.equal(a.window.SD.state.history.length, 2);
    assert.equal(a.window.SD.state.directory.baseline_id, 'latest');
    assert.equal(a.window.SD.state.comparison.target_current, true);
    assert.match(a.el('toast').textContent, /未新增历史/);
  } finally { a.close(); }
});

test('capture appends a node, selects it as default, and leaves the before/after result visible', async () => {
  const a = await app();
  try {
    await a.window.SD.app.openDirectory('dir');
    a.el('capture-btn').click(); await turn(); await a.completeScan();
    assert.equal(a.pending.request.mode, 'capture');
    assert.equal(a.window.SD.state.history.length, 3);
    assert.equal(a.window.SD.state.directory.baseline_id, 'scanned');
    assert.equal(a.window.SD.state.comparison.old.id, 'latest');
    assert.equal(a.window.SD.state.comparison.new.id, 'scanned');
    assert.equal(a.el('target-select').value, 'scanned');
    assert.match(a.el('update-notice').textContent, /保留本次更新前后的差异/);
  } finally { a.close(); }
});

test('algorithm transition preserves old history and shows the scan notice without a false comparison', async () => {
  for (const mode of ['check', 'capture']) {
    const a = await app({ algorithm: 'md5' });
    try {
      await a.window.SD.app.openDirectory('dir');
      const comparisons = a.calls.filter(c => c.cmd === 'compare_states').length;
      const notice = mode === 'check' ? '本次已使用 BLAKE3 扫描，请更新快照建立 BLAKE3 基准。' : '已保存 BLAKE3 基准，原 MD5 历史保留。';
      a.el(mode === 'check' ? 'check-btn' : 'capture-btn').click(); await turn();
      await a.completeScan({ notice });
      assert.equal(a.window.SD.state.history.length, mode === 'check' ? 2 : 3);
      assert.equal(a.window.SD.state.directory.baseline_id, mode === 'check' ? 'latest' : 'scanned');
      assert.equal(a.window.SD.state.directory.current.hash_algorithm, 'blake3');
      assert.equal(a.window.SD.state.history.find(n => n.id === 'latest').hash_algorithm, 'md5');
      assert.equal(a.el('update-notice').textContent, notice);
      assert.equal(a.el('workspace-empty').textContent, notice);
      assert(a.el('diff-section').classList.contains('hidden'));
      assert.equal(a.calls.filter(c => c.cmd === 'compare_states').length, comparisons);
      a.window.SD.state.oldId = 'latest'; a.window.SD.state.target = 'current';
      await a.window.SD.app.compare();
      assert.match(a.el('workspace-empty').textContent, /不能比较/);
      assert.match(a.el('comparison-hint').textContent, /摘要算法不同/);
      assert.equal(a.calls.filter(c => c.cmd === 'compare_states').length, comparisons);
      a.window.SD.state.oldId = 'older'; a.window.SD.state.target = 'latest';
      await a.window.SD.app.compare();
      assert.equal(a.calls.filter(c => c.cmd === 'compare_states').length, comparisons + 1);
      assert.match(a.el('diff-list').textContent, /MD5（历史）/);
    } finally { a.close(); }
  }
});

test('switching to an incompatible algorithm releases a pending comparison and ignores its late response', async () => {
  let finish;
  const a = await app({ compareStates: () => new Promise(resolve => { finish = resolve; }) });
  try {
    await a.window.SD.app.openDirectory('dir', false);
    const state = a.window.SD.state;
    state.oldId = 'older'; state.target = 'latest';
    const pending = a.window.SD.app.compare();
    assert.equal(state.compareLoading, true);
    state.directory.current.hash_algorithm = 'md5';
    state.target = 'current';
    await a.window.SD.app.compare();
    assert.equal(state.compareLoading, false);
    assert.match(a.el('workspace-empty').textContent, /不能比较/);
    finish({ id: 'stale' });
    await pending;
    assert.equal(state.comparison, null);
    assert.equal(a.calls.filter(c => c.cmd === 'query_diff').length, 0);
    assert.equal(a.el('target-select').disabled, false);
    assert.equal(a.el('check-btn').disabled, false);
  } finally { a.close(); }
});

test('new digest details display the complete BLAKE3 value', async () => {
  const a = await app();
  try {
    await a.window.SD.app.openDirectory('dir');
    a.window.SD.state.oldId = 'older'; a.window.SD.state.target = 'latest';
    await a.window.SD.app.compare();
    assert.match(a.el('diff-list').textContent, /BLAKE3/);
    assert(a.el('diff-list').textContent.includes('a'.repeat(64)));
    assert(!a.el('diff-list').textContent.includes('MD5'));
  } finally { a.close(); }
});

test('offline history can be selected as either side without performing any scan', async () => {
  const a = await app({ current: false, available: false });
  try {
    assert.equal(a.window.document.querySelector('[data-update]').disabled, true);
    await a.window.SD.app.openDirectory('dir');
    assert.equal(a.el('compare-btn').disabled, true);
    assert.equal(a.el('check-btn').disabled, true);
    assert.equal(a.el('capture-btn').disabled, true);
    assert.match(a.el('workspace-empty').textContent, /离线比较/);
    assert.equal(a.calls.filter(c => c.cmd === 'scan_directory').length, 0);
    await a.window.SD.history.open();
    a.window.document.querySelector('[data-baseline="older"]').click(); await turn(); await turn();
    assert.equal(a.window.SD.state.directory.baseline_id, 'older');
    assert.equal(a.el('history-modal').open, false);
    a.el('target-select').value = 'latest'; a.el('target-select').dispatchEvent(new a.window.Event('change'));
    await turn(); await turn();
    const comparison = a.calls.filter(c => c.cmd === 'compare_states').at(-1);
    assert.deepEqual(comparison.args, { directoryId: 'dir', oldId: 'older', newId: 'latest' });
    assert(!a.el('diff-section').classList.contains('hidden'));
    assert.equal(a.el('compare-btn').disabled, false);
    assert.equal(a.calls.filter(c => c.cmd === 'scan_directory').length, 0);
  } finally { a.close(); }
});

test('pagination stays in the backend and untrusted paths render as text', async () => {
  const a = await app({ rowCount: 205 });
  try {
    await a.window.SD.app.openDirectory('dir');
    a.window.SD.state.oldId = 'older'; a.window.SD.state.target = 'latest'; await a.window.SD.app.compare();
    assert.equal(a.window.document.querySelectorAll('.diff-row').length, 100);
    assert.equal(a.window.document.querySelector('#diff-list img'), null);
    assert.match(a.el('diff-list').textContent, /<img onerror="bad">/);
    a.el('next-page').click(); await turn();
    assert.equal(a.calls.filter(c => c.cmd === 'query_diff').at(-1).args.query.page, 1);
    assert.equal(a.el('page-label').textContent, '2 / 3');
    a.el('copy-all-btn').click(); await turn();
    assert.match(a.window.copiedText, /mods\/204.bin/);
  } finally { a.close(); }
});

test('adding focus updates cached results without executing retention or rebuilding historical comparison', async () => {
  const a = await app();
  try {
    await a.window.SD.app.openDirectory('dir');
    a.window.SD.state.oldId = 'older'; a.window.SD.state.target = 'latest'; await a.window.SD.app.compare();
    const comparisons = a.calls.filter(c => c.cmd === 'compare_states').length;
    a.window.document.querySelector('[data-focus="0"]').click(); await turn(); await turn();
    const command = a.calls.find(c => c.cmd === 'set_focus_patterns');
    assert.equal(command.args.directoryId, 'dir');
    assert.equal(command.args.retention, undefined);
    assert.equal(a.calls.filter(c => c.cmd === 'update_settings').length, 0);
    assert.equal(a.calls.filter(c => c.cmd === 'compare_states').length, comparisons);
    assert(a.window.document.querySelector('[data-focus="0"]').classList.contains('focused'));
  } finally { a.close(); }
});

test('deleting all history keeps the directory and selects an empty-baseline state', async () => {
  const a = await app();
  try {
    await a.window.SD.app.openDirectory('dir'); await a.window.SD.history.open();
    a.el('select-all-nodes').checked = true; a.el('select-all-nodes').dispatchEvent(new a.window.Event('change'));
    a.el('delete-nodes-btn').click(); await turn();
    assert(a.el('confirm-modal').open);
    assert.equal(a.calls.filter(c => c.cmd === 'delete_nodes').length, 0);
    a.el('confirm-yes').click(); await turn(); await turn(); await turn();
    assert.equal(a.window.SD.state.directory.id, 'dir');
    assert.equal(a.window.SD.state.history.length, 0);
    assert.equal(a.window.SD.state.directory.baseline_id, null);
    assert.match(a.el('workspace-empty').textContent, /第一个时间节点/);
  } finally { a.close(); }
});

test('retention shows actual nodes to be removed and cancel leaves settings unchanged', async () => {
  const a = await app();
  try {
    await a.window.SD.app.openDirectory('dir'); a.el('settings-btn').click();
    a.el('retention-mode').value = 'latest';
    a.el('settings-form').dispatchEvent(new a.window.Event('submit', { cancelable: true }));
    await turn();
    assert(a.el('confirm-modal').open);
    assert.match(a.el('confirm-details').textContent, /2026/);
    assert.equal(a.calls.filter(c => c.cmd === 'update_settings').length, 0);
    a.el('confirm-no').click(); await turn();
    assert.equal(a.calls.filter(c => c.cmd === 'update_settings').length, 0);
    assert(a.el('settings-modal').open);
  } finally { a.close(); }
});

test('import commits the exact preview ticket only after confirmation', async () => {
  const a = await app();
  try {
    a.el('import-btn').click(); await turn();
    assert(a.el('confirm-modal').open);
    assert.equal(a.calls.filter(c => c.cmd === 'import_snapshots').length, 0);
    a.el('confirm-yes').click(); await turn(); await turn();
    assert.deepEqual(a.calls.find(c => c.cmd === 'import_snapshots').args, { ticketId: 'immutable-ticket' });
  } finally { a.close(); }
});

test('creation has no naming field, closes its form while scanning, and retries the same mode', async () => {
  const a = await app();
  try {
    a.el('new-snapshot-btn').click();
    assert.equal(a.el('snapshot-name'), null);
    a.el('root-path').value = 'C:/new-folder';
    a.el('create-form').dispatchEvent(new a.window.Event('submit', { cancelable: true }));
    await turn();
    assert.equal(a.el('create-modal').open, false);
    assert.equal(a.pending.request.mode, 'create');
    a.pending.reject('该目录已经存在，请使用对应目录的更新按钮'); await turn();
    assert.match(a.el('progress-text').textContent, /更新/);
    assert(!a.el('retry-scan-btn').classList.contains('hidden'));
    a.el('retry-scan-btn').click(); await turn();
    assert.equal(a.pending.request.rootPath, 'C:/new-folder');
    assert.equal(a.pending.request.mode, 'create');
    a.pending.reject('扫描已取消'); await turn();
  } finally { a.close(); }
});
