'use strict';
SD.results = (() => {
  const { $, state: s, invoke, esc, date, bytes, delta, toast, action } = SD;
  const labels = { added: '新增', deleted: '删除', modified: '修改', moved: '疑似移动', metadata: '仅元数据' };
  const kinds = { file: '文件', directory: '目录', symlink: '链接' };
  let reportContext = null;

  function resetFilters() {
    s.query = { search: '', status: null, includeMetadata: false, focusedOnly: false, sort: 'path', tree: false, prefix: '', page: 0 };
    $('result-search').value = ''; $('include-metadata').checked = false;
    $('focused-only').checked = false; $('result-sort').value = 'path';
    $('list-mode').classList.add('selected'); $('tree-mode').classList.remove('selected');
  }
  function clear() {
    s.comparison = null; s.page = null; s.queryRequest++;
    $('diff-section').classList.add('hidden'); $('workspace-empty').classList.remove('hidden');
    SD.notice('scope-warning', '');
  }
  async function display(summary) {
    s.comparison = summary; s.oldId = summary.old.id;
    s.target = summary.target_current ? 'current' : summary.new.id;
    SD.app.renderSides();
    $('workspace-empty').classList.add('hidden'); $('diff-section').classList.remove('hidden');
    const warnings = [];
    if (summary.scope_unknown) warnings.push('旧版节点未保存完整忽略规则，新增和删除可能包含扫描范围变化。新节点会记录完整规则。');
    if (summary.scope_changed) warnings.push('两次忽略规则不同，只比较共同纳入的范围；排除 ' + summary.excluded_scope_count + ' 项，未将范围变化算作新增或删除。');
    if (!summary.directory_structure_available) warnings.push('比较包含旧版快照，旧节点未记录空目录，本次不统计目录结构差异。');
    SD.notice('scope-warning', warnings.join(' '));
    renderCounts();
    await load();
  }
  function renderCounts() {
    if (!s.comparison) return;
    const counts = s.comparison.counts;
    const total = counts.added + counts.deleted + counts.modified + counts.moved + (s.query.includeMetadata ? counts.metadata : 0);
    $('diff-counts').innerHTML =
      '<button class="metric all ' + (!s.query.status ? 'selected' : '') + '" data-status=""><span>全部差异</span><strong>' + total.toLocaleString() + '</strong></button>' +
      Object.entries(labels).map(([status, label]) => '<button class="metric ' + status + (s.query.status === status ? ' selected' : '') + '" data-status="' + status + '"><span>' + label + '</span><strong>' + counts[status].toLocaleString() + '</strong></button>').join('') +
      '<div class="metric size-metric"><span>大小变化 · 可比较范围</span><strong>' + delta(s.comparison.size_delta) + '</strong></div>';
    $('diff-counts').querySelectorAll('[data-status]').forEach(button => button.onclick = () => {
      s.query.status = button.dataset.status || null;
      if (s.query.status === 'metadata') { s.query.includeMetadata = true; $('include-metadata').checked = true; }
      s.query.page = 0; renderCounts(); load();
    });
  }
  async function load() {
    if (!s.comparison) return;
    const request = ++s.queryRequest, comparisonId = s.comparison.id;
    $('diff-list').innerHTML = '<div class="empty-list">正在读取差异…</div>';
    $('prev-page').disabled = $('next-page').disabled = true;
    try {
      const page = await invoke('query_diff', { comparisonId, query: { ...s.query } });
      if (request !== s.queryRequest || s.comparison?.id !== comparisonId) return;
      s.page = page; s.query.page = page.page; renderPage();
    } catch (error) {
      if (request !== s.queryRequest) return;
      $('diff-list').textContent = String(error); toast(String(error), true);
    }
  }
  function kindOf(row) { return row.new?.kind || row.old?.kind || 'file'; }
  function fullPath(path) {
    const root = s.directory.root_path.replace(/[\\/]$/, '');
    return root + (/^[a-z]:/i.test(root) || root.includes('\\') ? '\\' + path.replace(/\//g, '\\') : '/' + path);
  }
  function detail(row) {
    const fields = [
      ['相对路径', row.old?.relative_path, row.new?.relative_path],
      ['类型', row.old && kinds[row.old.kind || 'file'], row.new && kinds[row.new.kind || 'file']],
      ['文件大小', row.old && bytes(row.old.size), row.new && bytes(row.new.size)],
      ['修改时间', row.old && date(row.old.modified_time), row.new && date(row.new.modified_time)],
      [kindOf(row) === 'symlink' ? '链接目标路径 MD5' : 'MD5', row.old?.md5, row.new?.md5],
    ];
    return '<div class="file-details"><table><thead><tr><th>属性</th><th>旧状态</th><th>新状态</th></tr></thead><tbody>' +
      fields.map(([label, old, next]) => '<tr class="' + (old !== next ? 'changed-field' : '') + '"><th>' + label + '</th><td>' + esc(old || '—') + '</td><td>' + esc(next || '—') + '</td></tr>').join('') +
      '</tbody></table><div class="detail-actions"><button class="text-button" data-copy-path="' + esc(row.path) + '">复制相对路径</button><button class="text-button" data-copy-full="' + esc(row.path) + '">复制完整路径</button></div></div>';
  }
  function renderPage() {
    const page = s.page;
    renderBreadcrumb();
    let html = page.folders.map(folder => {
      const count = Object.values(folder.counts).reduce((a, b) => a + b, 0);
      const extra = (folder.counts.moved ? ' · 疑似移动 ' + folder.counts.moved : '') + (folder.counts.metadata ? ' · 仅元数据 ' + folder.counts.metadata : '');
      return '<button class="folder-row" data-folder="' + esc(folder.path) + '"><span class="folder-icon">▸</span><span class="folder-info"><strong>' + esc(folder.path.split('/').pop()) + '</strong><small>' + count + ' 项变化 · 新增 ' + folder.counts.added + ' · 删除 ' + folder.counts.deleted + ' · 修改 ' + folder.counts.modified + extra + '</small></span><span class="size-cell">' + delta(folder.size_delta) + '</span></button>';
    }).join('');
    html += page.rows.map((row, i) => {
      const kind = kindOf(row);
      const name = s.query.tree ? row.path.slice(s.query.prefix ? s.query.prefix.length + 1 : 0) : row.path;
      return '<details class="diff-row ' + row.status + '"><summary><span class="status-badge">' + labels[row.status] + '</span><span class="file-info"><span class="file-path" title="' + esc(row.path) + '">' + esc(name) + '</span><small>' + kinds[kind] + (row.status === 'moved' ? ' · 原路径 ' + esc(row.old.relative_path) : '') + '</small></span><span class="size-cell">' + delta(row.size_delta) + '</span><button class="focus-button ' + (row.focused ? 'focused' : '') + '" data-focus="' + i + '" title="' + (row.focused ? '已匹配关注规则；点击打开规则设置' : '重点关注此路径') + '" aria-label="关注路径">' + (row.focused ? '★' : '☆') + '</button><span class="detail-chevron">⌄</span></summary>' + detail(row) + '</details>';
    }).join('');
    $('diff-list').innerHTML = html || '<div class="empty-list">' + (s.query.search || s.query.status || s.query.focusedOnly ? '没有匹配的差异。可以调整筛选条件。' : '可比较范围内没有文件内容或目录结构变化。') + '</div>';
    $('diff-list').querySelectorAll('[data-folder]').forEach(button => button.onclick = () => { s.query.prefix = button.dataset.folder; s.query.page = 0; load(); });
    $('diff-list').querySelectorAll('[data-copy-path]').forEach(button => button.onclick = () => action(button, () => SD.copy(button.dataset.copyPath)));
    $('diff-list').querySelectorAll('[data-copy-full]').forEach(button => button.onclick = () => action(button, () => SD.copy(fullPath(button.dataset.copyFull))));
    $('diff-list').querySelectorAll('[data-focus]').forEach(button => button.onclick = event => {
      event.preventDefault();
      const row = page.rows[Number(button.dataset.focus)];
      if (row.focused) { SD.app.openSettings(); return; }
      action(button, async () => {
        const directory = s.directory;
        const pattern = '/' + row.path.replace(/[\[\]*?{}]/g, c => '[' + c + ']');
        const focus = [...directory.focus_patterns, pattern];
        await invoke('set_focus_patterns', { directoryId: directory.id, focusPatterns: focus });
        directory.focus_patterns = focus; toast('已加入重点关注');
        await load();
      });
    });
    $('result-total').textContent = '共 ' + page.total.toLocaleString() + (s.query.tree ? ' 个当前层级项目' : ' 条筛选结果') + ' · 每页 ' + page.page_size + ' 条';
    const pages = Math.max(1, Math.ceil(page.total / page.page_size));
    $('page-label').textContent = (page.page + 1) + ' / ' + pages;
    $('prev-page').disabled = page.page === 0;
    $('next-page').disabled = page.page + 1 >= pages;
    SD.app.syncBusy();
  }
  function renderBreadcrumb() {
    $('tree-breadcrumb').classList.toggle('hidden', !s.query.tree);
    if (!s.query.tree) return;
    const pieces = s.query.prefix.split('/').filter(Boolean);
    $('tree-breadcrumb').innerHTML = '<button data-prefix="">根目录</button>' + pieces.map((piece, i) => ' / <button data-prefix="' + esc(pieces.slice(0, i + 1).join('/')) + '">' + esc(piece) + '</button>').join('');
    $('tree-breadcrumb').querySelectorAll('[data-prefix]').forEach(button => button.onclick = () => { s.query.prefix = button.dataset.prefix; s.query.page = 0; load(); });
  }
  async function openReport() {
    if (!s.comparison) return;
    const id = s.comparison.id, query = { ...s.query };
    const report = await invoke('get_report', { comparisonId: id, query });
    if (s.comparison?.id !== id) return;
    reportContext = { comparisonId: id, query };
    $('report-text').value = report.text;
    $('copy-report-btn').disabled = report.bytes > 8192;
    $('copy-report-btn').title = report.bytes > 8192 ? '报告较大，请保存为文本文件' : '复制报告';
    SD.open('report-modal');
  }
  function bind() {
    let searchTimer;
    $('result-search').oninput = () => { clearTimeout(searchTimer); searchTimer = setTimeout(() => { s.query.search = $('result-search').value; s.query.page = 0; load(); }, 180); };
    $('result-sort').onchange = () => { s.query.sort = $('result-sort').value; s.query.page = 0; load(); };
    $('include-metadata').onchange = () => {
      s.query.includeMetadata = $('include-metadata').checked;
      if (!s.query.includeMetadata && s.query.status === 'metadata') s.query.status = null;
      s.query.page = 0; renderCounts(); load();
    };
    $('focused-only').onchange = () => { s.query.focusedOnly = $('focused-only').checked; s.query.page = 0; load(); };
    for (const [id, tree] of [['list-mode', false], ['tree-mode', true]]) {
      $(id).onclick = () => {
        s.query.tree = tree; s.query.prefix = ''; s.query.page = 0;
        $('list-mode').classList.toggle('selected', !tree); $('tree-mode').classList.toggle('selected', tree); load();
      };
    }
    $('prev-page').onclick = () => { s.query.page--; load(); };
    $('next-page').onclick = () => { s.query.page++; load(); };
    $('copy-all-btn').onclick = () => action($('copy-all-btn'), async () => {
      const paths = await invoke('get_diff_paths', { comparisonId: s.comparison.id, query: { ...s.query }, absolute: false }); await SD.copy(paths);
    });
    $('report-btn').onclick = () => action($('report-btn'), openReport);
    $('copy-report-btn').onclick = () => action($('copy-report-btn'), () => SD.copy($('report-text').value));
    $('save-report-btn').onclick = () => action($('save-report-btn'), async () => {
      const path = await SD.chooseSave('SnapDiff_差异报告.txt', 'txt');
      if (!path) return;
      await invoke('export_report', { ...reportContext, path });
      toast('报告已保存');
    });
  }
  return { bind, display, clear, resetFilters, renderCounts, load };
})();
