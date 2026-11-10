// Bridges one browser WebSocket to archond's Unix socket, line for line,
// as the remote listener would: same allowlist, hello as the CLI client.
import { createConnection } from 'node:net';
import assert from 'node:assert/strict';

export const ALLOWED_COMMANDS = new Set([
  'hello', 'status', 'repo_list', 'list_items', 'list_item_summaries', 'get_item', 'get_stage',
  'resolve_item', 'list_pipelines', 'list_questions', 'feed_history', 'subscribe_feed', 'list_reports',
  'orchestrator_status', 'cost_report', 'list_artifacts', 'read_artifact', 'item_diff', 'settings',
  'autonomy', 'enqueue', 'cancel_item', 'retry_item', 'pause', 'resume', 'answer', 'say',
]);
const MAX_FRAME_BYTES = 1 << 20;

export function bridgeSocket(ws, socketPath) {
  assert.ok(typeof socketPath === 'string' && socketPath.length > 0, 'the bridge needs the archond socket path');
  const daemon = createConnection(socketPath);
  let buffered = '';
  const toBrowser = (frame) => {
    if (ws.readyState === ws.OPEN) ws.send(typeof frame === 'string' ? frame : JSON.stringify(frame));
  };
  daemon.setEncoding('utf8');
  daemon.on('data', (chunk) => {
    buffered += chunk;
    const lines = buffered.split('\n');
    buffered = lines.pop();
    for (const line of lines) if (line.trim()) toBrowser(line);
  });
  daemon.on('error', (err) => {
    console.error(`dev bridge: archond socket ${socketPath}: ${err.message}`);
    ws.close(1011, 'archond is unreachable');
  });
  daemon.on('close', () => ws.close(1000, 'archond closed the connection'));
  ws.on('message', (data, isBinary) => {
    const outcome = translate(data, isBinary);
    if (outcome.refuse) toBrowser(outcome.refuse);
    else if (outcome.close) ws.close(1008, outcome.close);
    else daemon.write(`${JSON.stringify(outcome.forward)}\n`);
  });
  ws.on('close', () => daemon.end());
}

// One browser frame becomes a frame for archond, a refusal, or a close.
export function translate(data, isBinary = false) {
  if (isBinary || data.length > MAX_FRAME_BYTES) return { close: 'frames are JSON text under 1 MiB' };
  let frame;
  try {
    frame = JSON.parse(String(data));
  } catch {
    return { close: 'a frame is not JSON' };
  }
  if (!frame || typeof frame !== 'object' || Array.isArray(frame)) return { close: 'a frame is not an object' };
  if (!Number.isSafeInteger(frame.req) || frame.req < 1) return { close: 'a frame has no positive req id' };
  if (!ALLOWED_COMMANDS.has(frame.cmd)) {
    return { refuse: { frame: 'reply', id: frame.req, result: { Err: { code: 'forbidden', message: `forbidden: ${String(frame.cmd)} is not allowed from the web app` } } } };
  }
  // archond knows only its own client kinds; the remote listener speaks as the CLI.
  if (frame.cmd === 'hello') return { forward: { ...frame, client: 'archon-cli' } };
  return { forward: frame };
}
