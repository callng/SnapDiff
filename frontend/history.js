'use strict';
SD.history = (() => {
  const { $, state: s, invoke, esc, date, bytes, toast, action } = SD;
  let selected = new Set(), days = [], selectedDay = '', requestId = 0, remarkId = null;
  const visibleNodes = () => s.history.filter(node => !selectedDay || SD.localDay(node.created_time) === selectedDay);

  async function open() {
    if (!s.directory) return;
    selected = new Set(); selectedDay = '';
    $('history-root').textContent = s.directory.name + ' · ' + s.directory.root_path;
    $('history-list').innerHTML = '<div class="empty-list">正在读取时间节点…</div>';
    SD.open('history-modal');
    await reload();
  }
  async function reload() {
    const request = ++requestId, id = s.directory.id;
    const [nodes, activity] = await Promise.all([invoke('list_history', { directoryId: id }), invoke('get_activity', { directoryId: id })]);
    if (request !== requestId || s.directory?.id !== id) return;
    s.history = nodes; days = activity;
    const valid = new Set(nodes.map(node => node.id));
    selected = new Set([...selected].filter(id => valid.has(id)));
    renderWall(); renderNodes();
  }
  function renderWall() {
    const nodeDates = new Map();
    for (const node of s.history) {
      const day = SD.localDay(node.created_time);
      nodeDates.set(day, (nodeDates.get(day) || 0) + 1);
    }
    $('activity-wall').innerHTML = days.map(day => {
      const captures = Math.max(day.captures, nodeDates.get(day.date) || 0);
      const level = !day.scans ? 0 : day.changes === 0 ? 1 : day.changes < 10 ? 2 : day.changes < 100 ? 3 : 4;
      const label = day.date + ' · ' + (day.scans ? day.scans + ' 次扫描，发现差异 ' + day.changes + ' 项（按次累计）' : '没有扫描活动统计') + (captures ? ' · 保存过快照' : '');
      return '<button class="activity-cell level-' + level + (captures ? ' captured' : '') + (selectedDay === day.date ? ' selected' : '') + '" data-day="' + day.date + '" title="' + esc(label) + '" aria-label="' + esc(label) + '"></button>';
    }).join('');
    $('activity-wall').querySelectorAll('[data-day]').forEach(button => button.onclick = () => {
      selectedDay = selectedDay === button.dataset.day ? '' : button.dataset.day;
      $('activity-description').textContent = selectedDay ? button.title : '空格表示未观察；颜色表示手动扫描发现的差异，按次累计。圆点表示保存过快照。';
      renderWall(); renderNodes();
    });
    $('clear-day').classList.toggle('hidden', !selectedDay);
  }
  function renderNodes() {
    const nodes = visibleNodes();
    $('history-count').textContent = (selectedDay ? selectedDay + ' · ' : '') + nodes.length + ' 个时间节点';
    $('history-list').innerHTML = nodes.map(node => {
      const baseline = node.id === s.oldId, latest = node.id === s.history[0]?.id;
      return '<div class="history-item ' + (baseline ? 'selected' : '') + '"><input class="node-checkbox" type="checkbox" data-node="' + esc(node.id) + '" aria-label="选择 ' + esc(date(node.created_time)) + '" ' + (selected.has(node.id) ? 'checked' : '') + ' /><span class="history-marker ' + (latest ? 'latest' : '') + '"></span><button class="node-baseline" data-baseline="' + esc(node.id) + '"><strong>' + esc(date(node.created_time)) + (baseline ? ' <span class="node-badge">比较基准 ✓</span>' : '') + (latest ? ' <span class="latest-badge">最新</span>' : '') + '</strong><small>' + node.file_count.toLocaleString() + ' 个文件 · ' + node.directory_count.toLocaleString() + ' 个目录 · ' + bytes(node.total_size) + (!node.directory_entries_complete ? ' · 旧版记录' : '') + '</small>' + (node.remark ? '<span class="node-remark">' + esc(node.remark) + '</span>' : '') + '</button><div class="node-actions"><button class="text-button" data-target="' + esc(node.id) + '" ' + (baseline ? 'disabled' : '') + '>作为对照</button><button class="text-button" data-remark="' + esc(node.id) + '">备注</button></div></div>';
    }).join('') || '<div class="empty-list">' + (selectedDay ? '这一天没有保存的时间节点。' : '还没有历史节点。点击更新快照保存第一个状态。') + '</div>';
    $('history-list').querySelectorAll('[data-node]').forEach(input => input.onchange = () => {
      if (input.checked) selected.add(input.dataset.node); else selected.delete(input.dataset.node);
      renderSelection();
    });
    $('history-list').querySelectorAll('[data-baseline]').forEach(button => button.onclick = () => action(button, async () => {
      await invoke('select_baseline', { directoryId: s.directory.id, snapshotId: button.dataset.baseline });
      s.directory.baseline_id = s.oldId = button.dataset.baseline;
      SD.close('history-modal'); SD.notice('update-notice', '');
      await SD.app.compare();
    }));
    $('history-list').querySelectorAll('[data-target]').forEach(button => button.onclick = () => action(button, async () => {
      s.target = button.dataset.target; SD.close('history-modal'); SD.notice('update-notice', '');
      SD.app.renderSides(); await SD.app.compare();
    }));
    $('history-list').querySelectorAll('[data-remark]').forEach(button => button.onclick = () => {
      remarkId = button.dataset.remark; const node = s.history.find(n => n.id === remarkId);
      $('remark-time').textContent = date(node.created_time);
      $('remark-text').value = node.remark;
      SD.open('remark-modal'); $('remark-text').focus();
    });
    renderSelection(); SD.app.syncBusy();
  }
  function renderSelection() {
    $('history-selection-count').textContent = selected.size ? '已选中 ' + selected.size + ' 个节点' : '未选中节点';
    $('export-nodes-btn').disabled = $('delete-nodes-btn').disabled = !selected.size || !!s.task;
    const nodes = visibleNodes();
    const count = nodes.filter(node => selected.has(node.id)).length;
    $('select-all-nodes').checked = nodes.length > 0 && count === nodes.length;
    $('select-all-nodes').indeterminate = count > 0 && count < nodes.length;
  }
  function bind() {
    $('clear-day').onclick = () => { selectedDay = ''; $('activity-description').textContent = '空格表示未观察；颜色表示手动扫描发现的差异，按次累计。圆点表示保存过快照。'; renderWall(); renderNodes(); };
    $('select-all-nodes').onchange = () => {
      for (const node of visibleNodes()) { if ($('select-all-nodes').checked) selected.add(node.id); else selected.delete(node.id); }
      renderNodes();
    };
    $('delete-nodes-btn').onclick = () => action($('delete-nodes-btn'), async () => {
      const ids = [...selected], directoryId = s.directory.id;
      const details = s.history.filter(n => selected.has(n.id)).map(n => date(n.created_time) + (n.remark ? ' · ' + n.remark : '')).join('\n');
      if (!await SD.confirm('删除历史时间节点', '将删除 ' + ids.length + ' 个快照记录。目录中的实际文件不受影响。', details)) return;
      await invoke('delete_nodes', { directoryId, snapshotIds: ids }); selected.clear();
      await SD.app.refreshWorkspace(false); await reload(); toast('历史节点已删除');
    });
    $('export-nodes-btn').onclick = () => action($('export-nodes-btn'), () => SD.app.exportData(s.directory.id, [...selected]));
    $('remark-form').onsubmit = event => {
      event.preventDefault();
      action($('remark-form').querySelector('[type=submit]'), async () => {
        await invoke('set_remark', { directoryId: s.directory.id, snapshotId: remarkId, remark: $('remark-text').value });
        SD.close('remark-modal'); await reload(); SD.app.renderSides(); toast('备注已保存');
      });
    };
  }
  return { bind, open, reload };
})();