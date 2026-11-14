// Stands in for archond's `conversation` request, which needs a running
// orchestrator: Aether stays down in these fixtures, so it answers `unavailable`.
import { WEB_RELAY_MARK } from '../../src/core/conversation.js';

const POLL_STEP_MS = 100;
const POLL_HOLD_MS = 1_500;

export function conversationStandIn(seed = []) {
  const held = [];
  const push = (kind, text, extra = {}) => {
    const seq = (held.at(-1)?.seq ?? 0) + 1;
    held.push({ seq, part: 0, at: new Date().toISOString(), kind, important: true, text, ...extra });
  };
  for (const { kind, text, ...extra } of seed) push(kind, text, extra);
  const toSeq = () => held.at(-1)?.seq ?? 0;
  const send = (ws, id, data) => ws.send(JSON.stringify({ frame: 'reply', id, result: { Ok: { reply: 'conversation', data } } }));
  const page = (after) => ({
    entries: after === null ? held.slice(-200) : held.filter((e) => e.seq > after),
    from_seq: 0, to_seq: Math.max(after ?? 0, toSeq()), has_before: false, has_after: false, last_seq: toSeq(),
  });

  return {
    push,
    // Answers a `conversation` frame; a waiting poll is held briefly like the daemon's.
    answer(ws, frame) {
      const after = frame.after ?? null;
      const deadline = Date.now() + POLL_HOLD_MS;
      const tick = () => {
        const data = page(after);
        if (!frame.wait || data.entries.length > 0 || Date.now() >= deadline) send(ws, frame.req, data);
        else setTimeout(tick, POLL_STEP_MS);
      };
      tick();
    },
    // A phone line shows up as an operator entry once the orchestrator would read it.
    echoSay: (frame) => push('operator', `${WEB_RELAY_MARK}${frame.text}`),
  };
}

// Routes the page's socket through the stand-in; every other frame goes to archond.
export async function standInConversation(page, seed = []) {
  const standIn = conversationStandIn(seed);
  await page.routeWebSocket(/\/ws$/, (ws) => {
    const server = ws.connectToServer();
    server.onMessage((message) => ws.send(message));
    ws.onMessage((message) => {
      const frame = JSON.parse(message);
      if (frame.cmd === 'conversation') return standIn.answer(ws, frame);
      if (frame.cmd === 'say') standIn.echoSay(frame);
      return server.send(message);
    });
  });
  return standIn;
}
