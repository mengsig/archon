// Words and numbers as the app shows them: states, ages, money, models.
const ACTIVE = ['queued', 'dispatching', 'running', 'blocked'];
const TERMINAL = ['succeeded', 'failed', 'cancelled'];
const OPERATOR_REASONS = new Set(['operator_gate', 'dependency_failed', 'blocked']);

export function stateKind(item) {
  const kind = item?.state?.state;
  if (typeof kind !== 'string') throw new TypeError('an item has a state');
  return kind;
}

export function isTerminal(item) {
  return TERMINAL.includes(stateKind(item));
}

export function waitsOnOperator(summary) {
  if (isTerminal(summary)) return false;
  return Boolean(summary.waiting_on) || stateKind(summary) === 'blocked'
    || OPERATOR_REASONS.has(summary.waiting_reason?.code);
}

// Blocked, or waiting on something before it can run: a dependency, a gate, disk.
export function isHeldUp(summary) {
  if (isTerminal(summary)) return false;
  return stateKind(summary) === 'blocked' || Boolean(summary.waiting_reason);
}

const STATE_LOOK = {
  queued: { label: 'Queued', tone: 'neutral' },
  dispatching: { label: 'Starting', tone: 'accent', live: true },
  running: { label: 'Running', tone: 'accent', live: true },
  blocked: { label: 'Blocked', tone: 'amber' },
  succeeded: { label: 'Done', tone: 'green' },
  failed: { label: 'Failed', tone: 'red' },
  cancelled: { label: 'Cancelled', tone: 'muted' },
};

export function stateLook(summary) {
  if (waitsOnOperator(summary) && stateKind(summary) !== 'blocked') return { label: 'Needs you', tone: 'amber' };
  return STATE_LOOK[stateKind(summary)] ?? { label: stateKind(summary), tone: 'neutral' };
}

export const FILTERS = [
  { id: 'active', label: 'Active', test: (s) => ACTIVE.includes(stateKind(s)) },
  { id: 'waiting', label: 'Waiting on you', test: waitsOnOperator },
  { id: 'blocked', label: 'Held up', test: isHeldUp },
  { id: 'done', label: 'Done', test: (s) => ['succeeded', 'cancelled'].includes(stateKind(s)) },
  { id: 'failed', label: 'Failed', test: (s) => stateKind(s) === 'failed' },
  { id: 'all', label: 'All', test: () => true },
];
export const ACTIVE_STATES = ACTIVE;

const STAGE_LOOK = {
  spawning: { label: 'Starting', tone: 'accent', live: true },
  running: { label: 'Running', tone: 'accent', live: true },
  completed: { label: 'Completed', tone: 'green' },
  failed: { label: 'Failed', tone: 'red' },
  cancelled: { label: 'Cancelled', tone: 'muted' },
  timed_out: { label: 'Timed out', tone: 'red' },
};

export function stageLook(stage) {
  const kind = stage?.state?.state ?? 'unknown';
  return STAGE_LOOK[kind] ?? { label: kind, tone: 'neutral' };
}

export function verdictLook(verdict) {
  if (verdict === 'pass') return { label: 'Pass', tone: 'green' };
  if (verdict === 'fail') return { label: 'Fail', tone: 'red' };
  if (verdict === 'unknown') return { label: 'No verdict', tone: 'muted' };
  return null;
}

export function usd(value) {
  const n = Number(value ?? 0);
  if (!Number.isFinite(n)) return '—';
  if (n > 0 && n < 0.01) return '<$0.01';
  return `$${n.toFixed(2)}`;
}

export function modelName(choice) {
  const model = choice?.model;
  if (!model) return '';
  const name = typeof model === 'string' ? model : `${model.provider}/${model.model}`;
  return choice.effort ? `${name} · ${choice.effort}` : name;
}

export const PRIORITIES = ['critical', 'high', 'normal', 'low'];

export function priorityLabel(priority) {
  return { low: 'Low', normal: 'Normal', high: 'High', critical: 'Critical' }[priority] ?? String(priority);
}

const UNITS = [
  [60, 's', 1], [3600, 'm', 60], [86_400, 'h', 3600], [604_800, 'd', 86_400], [Infinity, 'w', 604_800],
];

// "4m", "3h", "2d": compact ages for lists.
export function age(iso, now = Date.now()) {
  const then = Date.parse(iso);
  if (!Number.isFinite(then)) return '';
  const secs = Math.max(0, Math.round((now - then) / 1000));
  if (secs < 45) return 'now';
  const [, unit, size] = UNITS.find(([limit]) => secs < limit);
  return `${Math.floor(secs / size)}${unit}`;
}

export function dateTime(iso) {
  const at = new Date(iso);
  if (Number.isNaN(at.getTime())) return '';
  return at.toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short' });
}

export function clockTime(iso) {
  const at = new Date(iso);
  if (Number.isNaN(at.getTime())) return '';
  return at.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
}

export function shortId(id) {
  const text = String(id ?? '');
  const cut = text.indexOf('_');
  return cut >= 0 ? `${text.slice(0, cut + 1)}${text.slice(cut + 1, cut + 9).toLowerCase()}` : text.slice(0, 10);
}

export function bytes(n) {
  if (!Number.isFinite(n)) return '';
  if (n < 1024) return `${n} B`;
  if (n < 1024 ** 2) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 ** 3) return `${(n / 1024 ** 2).toFixed(1)} MB`;
  return `${(n / 1024 ** 3).toFixed(1)} GB`;
}

export function safeHttpUrl(text) {
  try {
    const url = new URL(String(text));
    return url.protocol === 'https:' || url.protocol === 'http:' ? url.href : null;
  } catch {
    return null;
  }
}
