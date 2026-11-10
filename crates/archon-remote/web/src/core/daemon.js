// Every daemon command the app uses, with the exact request shape serde
// wants and a check of the reply kind that comes back.
import { DaemonError } from './transport.js';

function expect(...kinds) {
  return (reply) => {
    if (!kinds.includes(reply?.reply)) {
      throw new DaemonError('internal', `expected a ${kinds.join(' or ')} reply, got ${reply?.reply}`);
    }
    return reply.data;
  };
}

const ITEM_QUERY = { states: [], repo: null, pipeline: null, tag: null, limit: null };
export const ARTIFACT_PAGE_BYTES = 64 * 1024;

export function daemonCommands(request) {
  if (typeof request !== 'function') throw new TypeError('daemon commands need a request function');
  return {
    status: () => request('status').then(expect('status')),
    summaries: (query = {}) => request('list_item_summaries', { ...ITEM_QUERY, ...query }).then(expect('item_summaries')),
    item: (item) => request('get_item', { item }).then(expect('item_detail')),
    repos: () => request('repo_list').then(expect('repos')),
    pipelines: (repo = null) => request('list_pipelines', { repo }).then(expect('catalog')),
    questions: () => request('list_questions', { open_only: false }).then(expect('questions')),
    reports: () => request('list_reports').then(expect('reports')),
    artifacts: (item) => request('list_artifacts', { item }).then(expect('artifacts')),
    readArtifact: (item, name, offset = 0) =>
      request('read_artifact', { item, name, offset, limit: ARTIFACT_PAGE_BYTES }).then(expect('artifact_page')),
    costReport: (by = 'repo') => request('cost_report', { since: null, until: null, by }).then(expect('cost_report')),
    enqueue: (draft) => request('enqueue', enqueueFields(draft)).then(expect('item', 'item_budgets_off')),
    cancel: (item) => request('cancel_item', { item, reason: null }).then(expect('item')),
    retry: (item) => request('retry_item', { item, model_plan: null }).then(expect('item')),
    // A PauseTarget flattens beside `cmd`: {scope: global, hard} | {scope: repo|pipeline, name}.
    pause: (target) => request('pause', pauseFields(target)).then(expect('ok')),
    resume: (target) => request('resume', pauseFields(target)).then(expect('ok')),
    answer: (question, text) => request('answer', { question, text }).then(expect('question')),
    say: (text) => request('say', { text }).then(expect('ok', 'message_receipt')),
  };
}

function pauseFields(target) {
  if (target?.scope === 'global') return { scope: 'global', hard: false };
  if ((target?.scope === 'repo' || target?.scope === 'pipeline') && typeof target.name === 'string' && target.name) {
    return { scope: target.scope, name: target.name };
  }
  throw new TypeError(`not a pause target: ${JSON.stringify(target)}`);
}

function enqueueFields(draft) {
  if (typeof draft?.title !== 'string' || typeof draft?.brief !== 'string') throw new TypeError('an item needs a title and brief');
  const fields = {
    repo: draft.repo || null,
    pipeline: draft.pipeline || null,
    title: draft.title,
    brief: draft.brief,
    priority: draft.priority || 'normal',
    budget_usd: null,
    idempotency_key: draft.idempotencyKey ?? null,
  };
  if (draft.review) fields.autonomy = { review: draft.review };
  return fields;
}
