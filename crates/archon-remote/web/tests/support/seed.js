// Realistic data in a fresh archond, through the CLI and raw socket requests.
import assert from 'node:assert/strict';
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { gitRepo } from './archond.js';

export async function addRepo(archond, name) {
  const path = join(archond.home, 'repos', name);
  await gitRepo(path);
  await archond.run(['repo', 'add', path, '--name', name, '--ship', 'local']);
  return path;
}

export async function enqueue(archond, { repo, title, brief, priority = 'normal', pipeline, dependsOn }) {
  const briefFile = join(archond.home, `brief-${Date.now()}-${Math.random().toString(36).slice(2)}.md`);
  writeFileSync(briefFile, brief);
  const args = ['--json', 'queue', 'add', '--repo', repo, '--title', title, '--brief-file', briefFile, '--priority', priority];
  if (pipeline) args.push('--pipeline', pipeline);
  if (dependsOn) args.push('--depends-on', dependsOn);
  const { stdout } = await archond.run(args);
  const item = JSON.parse(stdout);
  assert.match(item.id, /^qi_/, 'queue add answers the new item');
  return item;
}

export async function askOperator(archond, text, options = []) {
  const reply = await archond.request({ cmd: 'ask_operator', text, item: null, options });
  assert.equal(reply.reply, 'question', 'ask_operator answers the recorded question');
  return reply.data;
}

export async function notify(archond, kind, text, item = null) {
  const reply = await archond.request({ cmd: 'notify', kind, text, item });
  assert.equal(reply.reply, 'ok', `notify answered ${reply.reply}`);
}

// Two repositories, a handful of items in varied states, two open questions.
export async function seedDemo(archond) {
  await addRepo(archond, 'archon');
  await addRepo(archond, 'website');
  const first = await enqueue(archond, {
    repo: 'archon', title: 'Add a remote listener for the phone app', priority: 'high',
    brief: 'Serve the web app and carry the socket protocol over a WebSocket.',
  });
  await enqueue(archond, {
    repo: 'archon', title: 'Pair devices with a one-time code', dependsOn: first.id,
    brief: 'A short code printed as a QR on the host terminal.',
  });
  const cancelled = await enqueue(archond, {
    repo: 'website', title: 'Refresh the landing page copy', priority: 'low',
    brief: 'Tighten the hero and the feature grid.',
  });
  await archond.run(['queue', 'cancel', cancelled.id]);
  await enqueue(archond, {
    repo: 'website', title: 'Ship dark mode for the docs', dependsOn: cancelled.id,
    brief: 'Follow the system theme with a manual override.',
  });
  await askOperator(archond, 'Ship the listener behind a feature flag first?', ['approve', 'reject']);
  await askOperator(archond, 'Which port should the remote listener default to?');
  await notify(archond, 'decision', 'Queued the pairing work after the listener, since it needs the session cookie.');
  return { first, cancelled };
}
