// A plain-HTML fixture app with live reload: the page listens on /events and
// the server pushes "reload" whenever style.css changes. Run: node server.js [port]
import { createServer } from 'node:http';
import { readFileSync, watch } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const DIR = dirname(fileURLToPath(import.meta.url));
const TYPES = { '/': 'text/html', '/style.css': 'text/css', '/app.js': 'text/javascript' };

export function startFixtureApp(port = 0) {
  const clients = new Set();
  const server = createServer((req, res) => {
    const path = req.url.split('?')[0];
    if (path === '/events') {
      res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-store' });
      clients.add(res);
      req.on('close', () => clients.delete(res));
      return;
    }
    if (!TYPES[path]) { res.writeHead(404).end(); return; }
    res.writeHead(200, { 'Content-Type': TYPES[path], 'Cache-Control': 'no-store' }).end(readFileSync(join(DIR, path === '/' ? 'index.html' : path.slice(1))));
  });
  const watcher = watch(join(DIR, 'style.css'), () => clients.forEach((c) => c.write('data: reload\n\n')));
  return new Promise((resolve) => server.listen(port, '127.0.0.1', () => resolve({
    url: `http://127.0.0.1:${server.address().port}/`,
    css: join(DIR, 'style.css'),
    stop: () => new Promise((done) => { watcher.close(); clients.forEach((c) => c.end()); server.close(done); }),
  })));
}

if (process.argv[1] === fileURLToPath(import.meta.url)) startFixtureApp(Number(process.argv[2] ?? 5173)).then((app) => console.log(app.url));
