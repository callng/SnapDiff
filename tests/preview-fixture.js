(() => {
  const clone = value => JSON.parse(JSON.stringify(value));
  const nodes = [
    { id: 'latest', created_time: '2026-10-12T02:26:48Z', remark: '调整配置后', file_count: 14283, directory_count: 841, total_size: 76450488320 },
    { id: 'before', created_time: '2026-10-11T06:17:32Z', remark: '新增插件后', file_count: 14265, directory_count: 839, total_size: 76450002320 },
    { id: 'clean', created_time: '2026-10-10T00:32:19Z', remark: '初始状态', file_count: 14260, directory_count: 837, total_size: 76448002320 },
  ].map(node => ({ ...node, hash_algorithm: 'blake3', directory_id: 'demo', directory_entries_complete: true, scan_started_time: node.created_time }));
  const directory = { id: 'demo', name: 'Workspace', root_path: 'D:\\Projects\\Workspace', root_key: 'd:/projects/workspace', baseline_id: 'clean', current_id: 'latest', ignore_patterns: ['*.log', 'cache/'], focus_patterns: ['/bin/plugin.dll'], retention: 0, history_count: 3, latest: nodes[0], current: nodes[0], current_types: { '.dll': { count: 129, size: 24450488320 }, '.pak': { count: 246, size: 51440000000 }, '.ini': { count: 92, size: 3200000 }, '.json': { count: 784, size: 73600000 } }, available: true };
  const file = (path, size, hash) => ({ relative_path: path, kind: 'file', size, hash: hash.repeat(64), modified_time: nodes[0].created_time });
  const rows = [
    { path: 'bin/plugin.dll', status: 'modified', old: file('bin/plugin.dll', 102400, 'a'), new: file('bin/plugin.dll', 155648, 'b'), size_delta: 53248, focused: true },
    { path: 'config/settings.ini', status: 'modified', old: file('config/settings.ini', 1421, 'a'), new: file('config/settings.ini', 1582, 'c'), size_delta: 161, focused: false },
    { path: 'plugins/custom/extension.dll', status: 'added', old: null, new: file('plugins/custom/extension.dll', 204800, 'd'), size_delta: 204800, focused: false },
    { path: 'plugins/custom/manifest.json', status: 'added', old: null, new: file('plugins/custom/manifest.json', 480, 'd'), size_delta: 480, focused: false },
    { path: 'old/helper.dll', status: 'deleted', old: file('old/helper.dll', 18432, 'e'), new: null, size_delta: -18432, focused: false },
    { path: 'assets/new/settings.json', status: 'moved', old: file('assets/settings.json', 1900, 'f'), new: file('assets/new/settings.json', 1900, 'f'), size_delta: 0, focused: false },
    { path: 'cache-marker.dat', status: 'metadata', old: file('cache-marker.dat', 512, 'a'), new: file('cache-marker.dat', 512, 'a'), size_delta: 0, focused: false },
    { path: 'empty-folder', status: 'added', old: null, new: { relative_path: 'empty-folder', kind: 'directory', size: 0, hash: '', modified_time: nodes[0].created_time }, size_delta: 0, focused: false },
  ];
  const activity = Array.from({ length: 120 }, (_, i) => {
    const day = new Date('2026-10-12T00:00:00'); day.setDate(day.getDate() - 119 + i);
    const date = [day.getFullYear(), String(day.getMonth() + 1).padStart(2, '0'), String(day.getDate()).padStart(2, '0')].join('-');
    return { date, scans: i % 13 === 0 || i > 116 ? 1 : 0, captures: i % 13 === 0 || i > 116 ? 1 : 0, changes: i % 13 === 0 ? (i * 7) % 180 : i > 116 ? 8 : 0 };
  });
  window.__TAURI__ = {
    core: { invoke: async (command, args = {}) => {
      switch (command) {
        case 'list_directories': return [clone(directory)];
        case 'get_directory': return clone(directory);
        case 'list_history': return clone(nodes);
        case 'get_storage_info': return { warnings: [], mode: 'default', database_path: 'C:\\Demo\\SnapDiff\\snapdiff.sqlite3', directory: 'C:\\Demo\\SnapDiff', default_directory: 'C:\\Demo\\SnapDiff', install_directory: 'D:\\Demo\\SnapDiff\\data' };
        case 'migrate_database': throw new Error('演示预览不会迁移实际数据库，请在桌面应用中操作');
        case 'get_activity': return clone(activity);
        case 'select_baseline': directory.baseline_id = args.snapshotId; return;
        case 'compare_states': return { id: 'demo-comparison', directory_id: directory.id, old: clone(nodes.find(n => n.id === args.oldId)), new: clone(nodes.find(n => n.id === args.newId) || nodes[0]), target_current: !args.newId, counts: { added: 3, deleted: 1, modified: 2, moved: 1, metadata: 1 }, size_delta: 240257, scope_changed: false, scope_unknown: false, excluded_scope_count: 0, directory_structure_available: true };
        case 'query_diff': {
          const q = args.query;
          let selected = rows.filter(r => (q.includeMetadata || r.status !== 'metadata') && (!q.status || r.status === q.status) && (!q.focusedOnly || r.focused) && (!q.search || r.path.includes(q.search)) && (!q.prefix || r.path.startsWith(q.prefix + '/')));
          const folders = [];
          if (q.tree) {
            const groups = new Map();
            selected = selected.filter(row => {
              const rest = q.prefix ? row.path.slice(q.prefix.length + 1) : row.path;
              if (!rest.includes('/')) return true;
              const path = (q.prefix ? q.prefix + '/' : '') + rest.split('/')[0];
              if (!groups.has(path)) groups.set(path, { path, counts: { added: 0, deleted: 0, modified: 0, moved: 0, metadata: 0 }, size_delta: 0 });
              const folder = groups.get(path); folder.counts[row.status]++; folder.size_delta += row.size_delta; return false;
            });
            folders.push(...groups.values());
          }
          return { rows: clone(selected), folders, total: selected.length + folders.length, page: 0, page_size: 100 };
        }
        case 'preview_retention': return clone(args.retention ? nodes.slice(args.retention) : []);
        case 'update_settings': directory.ignore_patterns = args.ignorePatterns; directory.focus_patterns = args.focusPatterns; directory.retention = args.retention; return 0;
        case 'get_report': return { text: 'SnapDiff 目录差异报告（界面预览）\n新增 3 · 删除 1 · 修改 2 · 疑似移动 1\n\nbin/plugin.dll\n旧 BLAKE3 ' + 'a'.repeat(64) + '\n新 BLAKE3 ' + 'b'.repeat(64), bytes: 180, truncated: false };
        case 'scan_directory': throw new Error('这是演示数据预览；实际扫描请运行 Tauri 应用。');
        default: return;
      }
    } },
    event: { listen: async () => () => {} },
  };
})();
