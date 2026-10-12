const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');
const root = path.join(__dirname, '..', 'frontend');
http.createServer((request, response) => {
  const url = new URL(request.url, 'http://127.0.0.1:47831');
  const name = url.pathname === '/' ? 'index.html' : url.pathname.slice(1);
  const allowed = ['index.html', 'styles.css', 'core.js', 'results.js', 'history.js', 'app.js', 'preview-fixture.js'];
  if (!allowed.includes(name)) { response.writeHead(404); response.end(); return; }
  let content = fs.readFileSync(name === 'preview-fixture.js' ? path.join(__dirname, name) : path.join(root, name), 'utf8');
  if (name === 'index.html') content = content.replace('<head>', '<head><script src="/preview-fixture.js"></script>').replace('<main>', '<main><div class="notice">界面检查预览 · 使用演示数据，不执行实际目录扫描</div>');
  response.writeHead(200, { 'Content-Type': name.endsWith('.css') ? 'text/css; charset=utf-8' : name.endsWith('.js') ? 'text/javascript; charset=utf-8' : 'text/html; charset=utf-8' });
  response.end(content);
}).listen(47831, '127.0.0.1', () => process.stdout.write('UI preview: http://127.0.0.1:47831\n'));