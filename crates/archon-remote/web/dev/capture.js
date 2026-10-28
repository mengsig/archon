// Seed a private archond and print the real replies the app is built against:
// `node dev/capture.js > frames.txt` (ARCHON_BIN or CARGO_TARGET_DIR names the binary).
import { startArchond } from '../tests/support/archond.js';
import { seedDemo } from '../tests/support/seed.js';

const archond = await startArchond();
try {
  const { first } = await seedDemo(archond);
  const cmds = [
    { cmd: 'status' },
    { cmd: 'list_item_summaries', states: [], repo: null, pipeline: null, tag: null, limit: null },
    { cmd: 'get_item', item: first.id },
    { cmd: 'get_stage', item: first.id, stage: null, attempt: null, slot: null },
    { cmd: 'list_questions', open_only: false },
    { cmd: 'feed_history', before: null, limit: 200, verbose: false },
    { cmd: 'repo_list' },
    { cmd: 'list_pipelines', repo: null },
    { cmd: 'list_pipelines', repo: 'archon' },
    { cmd: 'orchestrator_status' },
    { cmd: 'cost_report' },
    { cmd: 'list_reports' },
    { cmd: 'list_artifacts', item: first.id },
    { cmd: 'item_diff', item: first.id },
    { cmd: 'pause', scope: 'global', hard: false },
    { cmd: 'resume', scope: 'global', hard: false },
    { cmd: 'say', text: 'hello from capture' },
    { cmd: 'enqueue', repo: 'archon', pipeline: null, title: '', brief: 'x', budget_usd: null, idempotency_key: null },
    { cmd: 'enqueue', repo: 'archon', pipeline: null, title: 'Captured', brief: 'x', priority: 'high', budget_usd: null, idempotency_key: null, autonomy: { review: 'strict' } },
  ];
  for (const cmd of cmds) {
    const out = await archond.request(cmd).catch((err) => ({ ERROR: err.message }));
    console.log(`=== ${JSON.stringify(cmd)}\n${JSON.stringify(out, null, 1)}`);
  }
} finally {
  await archond.stop();
}
