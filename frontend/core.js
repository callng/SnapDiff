'use strict';
// Cancel WebView's default menu, including inputs and keyboard menu requests.
document.addEventListener('contextmenu', event => event.preventDefault(), { capture: true });
// File paths come from Tauri's native drop event; never navigate to dropped files.
document.addEventListener('dragover', event => event.preventDefault());
document.addEventListener('drop', event => event.preventDefault());
window.SD = (() => {
  const $ = id => document.getElementById(id);
  const invoke = (command, args) => window.__TAURI__?.core?.invoke
    ? window.__TAURI__.core.invoke(command, args)
    : Promise.reject(new Error('请在 SnapDiff 桌面应用中操作'));
  const esc = value => String(value ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#039;' }[c]));
  const state = {
    directories: [], directory: null, history: [], oldId: null, target: 'current',
    comparison: null, page: null, query: { search: '', status: null, includeMetadata: false, focusedOnly: false, sort: 'path', tree: false, prefix: '', page: 0 },
    task: null, lastScanRequest: null, workspaceRequest: 0, compareRequest: 0, queryRequest: 0,
  };
  let toastTimer, confirmation;
  function toast(message, error = false) {
    const el = $('toast');
    el.textContent = String(message);
    el.classList.toggle('error', error);
    el.classList.add('show');
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => el.classList.remove('show'), 4500);
  }
  function date(value) {
    if (!value) return '尚未记录';
    return new Date(value).toLocaleString('zh-CN', { year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', second: '2-digit', hour12: false });
  }
  function localDay(value) {
    const d = new Date(value);
    return [d.getFullYear(), String(d.getMonth() + 1).padStart(2, '0'), String(d.getDate()).padStart(2, '0')].join('-');
  }
  function bytes(value) {
    const n = Number(value || 0), units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
    let size = Math.abs(n), i = 0;
    while (size >= 1024 && i < units.length - 1) { size /= 1024; i++; }
    return (n < 0 ? '−' : '') + (i ? size.toFixed(size >= 100 ? 1 : 2) : String(size)) + ' ' + units[i];
  }
  const delta = value => Number(value) > 0 ? '+' + bytes(value) : bytes(value);
  function open(id) { if (!$(id).open) $(id).showModal(); }
  function close(id) { if ($(id).open) $(id).close(); }
  function showView(name) { document.querySelectorAll('.view').forEach(el => el.classList.toggle('active', el.id === name + '-view')); }
  function notice(id, message) { $(id).textContent = message; $(id).classList.toggle('hidden', !message); }
  async function action(button, callback) {
    if (button?.disabled) return;
    if (button) { button.disabled = true; button.dataset.working = 'true'; }
    try { return await callback(); }
    catch (error) { toast(String(error), true); }
    finally { if (button) { button.disabled = false; delete button.dataset.working; } SD.app?.syncBusy(); }
  }
  async function copy(text) {
    if (!text) { toast('没有可复制的内容'); return; }
    try { await navigator.clipboard.writeText(text); }
    catch {
      const active = document.querySelector('dialog[open]') || document.body;
      const area = document.createElement('textarea');
      area.value = text; area.style.position = 'fixed'; area.style.opacity = '0'; active.appendChild(area); area.select();
      const ok = document.execCommand('copy'); area.remove();
      if (!ok) throw new Error('复制失败，请手动复制');
    }
    toast('已复制');
  }
  function confirm(title, message, details = '') {
    if (confirmation) return Promise.resolve(false);
    $('confirm-title').textContent = title;
    $('confirm-message').textContent = message;
    $('confirm-details').textContent = details;
    $('confirm-details').classList.toggle('hidden', !details);
    open('confirm-modal');
    return new Promise(resolve => { confirmation = resolve; });
  }
  function finishConfirm(answer) {
    if (!confirmation) return;
    const resolve = confirmation; confirmation = null;
    close('confirm-modal'); resolve(answer);
  }
  async function chooseDirectory() {
    if (!window.__TAURI__?.dialog) throw new Error('无法打开目录选择器');
    return window.__TAURI__.dialog.open({ directory: true, multiple: false, title: '选择要记录状态的目录' });
  }
  async function chooseOpen() {
    if (!window.__TAURI__?.dialog) throw new Error('无法打开文件选择器');
    return window.__TAURI__.dialog.open({ multiple: false, title: '导入快照数据', filters: [{ name: '快照数据', extensions: ['json'] }] });
  }
  async function chooseSave(name, extension) {
    if (!window.__TAURI__?.dialog) throw new Error('无法打开保存窗口');
    return window.__TAURI__.dialog.save({ defaultPath: name, filters: [{ name: extension === 'json' ? '快照数据' : '文字报告', extensions: [extension] }] });
  }
  const lines = text => text.split(/\r?\n/).map(s => s.trim()).filter(Boolean);
  function bindCommon() {
    document.querySelectorAll('[data-close]').forEach(button => button.onclick = () => close(button.dataset.close));
    $('confirm-yes').onclick = () => finishConfirm(true);
    $('confirm-no').onclick = $('confirm-close').onclick = () => finishConfirm(false);
    $('confirm-modal').addEventListener('cancel', event => { event.preventDefault(); finishConfirm(false); });
    $('confirm-modal').addEventListener('close', () => finishConfirm(false));
  }
  return { $, invoke, esc, state, toast, date, localDay, bytes, delta, open, close, showView, notice, action, copy, confirm, chooseDirectory, chooseOpen, chooseSave, lines, bindCommon };
})();
