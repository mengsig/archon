// A stand-in for Aether's `/archon` mount: it forwards /archon/* to archond's
// listener with the prefix removed, the Host set to the loopback target and
// the browser's Origin kept, including the WebSocket upgrade.
import { createServer, request } from 'node:http';
import { connect } from 'node:net';
import assert from 'node:assert/strict';

export const PREFIX = '/archon';

function stripPrefix(url) {
  assert.ok(url === PREFIX || url.startsWith(`${PREFIX}/`) || url.startsWith(`${PREFIX}?`), `not under the mount: ${url}`);
  const rest = url.slice(PREFIX.length);
  return rest.startsWith('/') ? rest : `/${rest}`;
}

// Listens on a free loopback port; `forwardTo(port)` names archond's listener.
export async function startMount() {
  let target = null;
  const server = createServer((req, res) => {
    if (target === null || !req.url.startsWith(PREFIX)) {
      res.writeHead(404).end('not mounted');
      return;
    }
    const headers = { ...req.headers, host: `127.0.0.1:${target}` };
    const upstream = request({ host: '127.0.0.1', port: target, method: req.method, path: stripPrefix(req.url), headers }, (reply) => {
      res.writeHead(reply.statusCode, reply.headers);
      reply.pipe(res);
    });
    upstream.on('error', (err) => res.writeHead(502).end(String(err)));
    req.pipe(upstream);
  });
  server.on('upgrade', (req, socket, head) => {
    if (target === null || !req.url.startsWith(PREFIX)) return socket.destroy();
    const upstream = connect(target, '127.0.0.1', () => {
      const lines = [`${req.method} ${stripPrefix(req.url)} HTTP/1.1`];
      for (let i = 0; i < req.rawHeaders.length; i += 2) {
        const name = req.rawHeaders[i];
        lines.push(`${name}: ${name.toLowerCase() === 'host' ? `127.0.0.1:${target}` : req.rawHeaders[i + 1]}`);
      }
      upstream.write(`${lines.join('\r\n')}\r\n\r\n`);
      upstream.write(head);
      socket.pipe(upstream).pipe(socket);
    });
    upstream.on('error', () => socket.destroy());
    socket.on('error', () => upstream.destroy());
  });
  await new Promise((done, fail) => server.once('error', fail).listen(0, '127.0.0.1', done));
  const { port } = server.address();
  return {
    origin: `http://127.0.0.1:${port}`,
    forwardTo: (archondPort) => { target = archondPort; },
    stop: () => new Promise((done) => { server.closeAllConnections(); server.close(done); }),
  };
}
