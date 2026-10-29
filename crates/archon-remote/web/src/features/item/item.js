// Item detail: state and actions, what it waits on, its stages, cost,
// pull request, artifacts and reports.
import { h } from '../../ui/dom.js';
import { icon } from '../../ui/icons.js';
import { screenHeader, section, card, row, chip, emptyState, errorNote, loadErrorNotes, skeletonRows } from '../../ui/components.js';
import { stateLook, usd, priorityLabel, dateTime, shortId, safeHttpUrl, isTerminal, waitsOnOperator, prUrlOf, namesItem } from '../../core/format.js';
import { stagesTimeline } from './stages.js';
import { artifactsSection } from './artifacts.js';
import { itemActions } from './actions.js';

const BRIEF_CLAMP_CHARS = 360;
const FOLLOW_DEBOUNCE_MS = 300;
const POLL_RUNNING_MS = 15_000;
const SHARED_LOADS = ['status', 'repos', 'summaries'];
const REVIEW_LABEL = { strict: 'Strict', standard: 'Standard', none: 'None' };
const GATE_SUBJECT = { proposal: 'Proposal', report: 'Report' };
const GATE_LOOK = {
  approve: { label: 'Approved', tone: 'green' }, reject: { label: 'Rejected', tone: 'red' },
  revise: { label: 'Revise', tone: 'amber' }, waiting: { label: 'Waiting on you', tone: 'amber' },
  withdrawn: { label: 'Withdrawn', tone: 'muted' },
};

export function mountItem(view, ctx) {
  const id = ctx.route.param;
  const back = { label: 'Back to queue', text: 'Queue', onClick: () => ctx.router.back('#/queue') };
  if (!/^qi_[A-Za-z0-9]+$/.test(id ?? '')) {
    view.append(screenHeader({ title: 'Item', back }), emptyState({ title: 'Not an item', text: 'That link does not name a queue item.' }));
    return () => {};
  }
  const content = h('div', { class: 'item-detail' }, skeletonRows(5));
  view.append(screenHeader({ title: 'Item', back }), content);
  let alive = true;
  let detail = null;
  let artifacts = null;
  let artifactsAt = null;
  let drawn = null;

  let loads = 0;
  let failed = false;
  // Only the newest of overlapping loads is shown; a quiet one that finds
  // nothing new keeps the page as the operator left it.
  const load = async (quiet) => {
    const ticket = ++loads;
    try {
      const fresh = await ctx.cmds.item(id);
      if (!alive || ticket !== loads) return;
      const unchanged = quiet && !failed && detail && JSON.stringify(fresh) === JSON.stringify(detail);
      detail = fresh;
      failed = false;
      if (!unchanged) render();
    } catch (err) {
      if (!alive || ticket !== loads) return;
      failed = true;
      content.replaceChildren(err.code === 'not_found'
        ? emptyState({ title: 'Item not found', text: 'It may have been removed from the queue.', iconName: 'queue' })
        : errorNote(err, reload));
    }
  };
  const reload = () => load(false);
  const render = () => {
    const state = ctx.store.get();
    // Listed again once a stage has finished since, as it may have published files.
    const finished = detail.stages.filter((s) => s.state?.state !== 'running' && s.state?.state !== 'spawning').length;
    if (artifactsAt !== finished) {
      artifacts = artifactsSection(ctx, id);
      artifactsAt = finished;
    }
    drawn = JSON.stringify(shownFromStore(state, id, detail));
    view.querySelector('.screen-title').textContent = detail.item.title;
    content.replaceChildren(...renderDetail(ctx, detail, state, { reload, artifacts }));
  };
  const unfollow = followItem(ctx, id, () => detail, () => load(true));
  // A failed reload keeps its error and retry in view until a load succeeds;
  // values the page already shows (the detail just arrived) do not redraw it.
  const offReports = ctx.store.watch((s) => shownFromStore(s, id, detail), (shown) => detail && !failed && JSON.stringify(shown) !== drawn && render());
  reload();
  return () => { alive = false; unfollow(); offReports(); };
}

// Stages settle and gates open without touching the item itself, so the
// detail reloads on feed entries about it and, while it runs, on a timer.
function followItem(ctx, id, current, refresh) {
  if (typeof refresh !== 'function' || typeof current !== 'function') throw new TypeError('followItem needs the detail and its refresh');
  let timer = null;
  const soon = () => {
    clearTimeout(timer);
    timer = setTimeout(() => current() && refresh(), FOLLOW_DEBOUNCE_MS);
  };
  const offFeed = ctx.feed.subscribe((fresh) => fresh.some((entry) => namesItem(entry, id)) && soon());
  const offSummary = ctx.store.watch((s) => summaryOf(s, id)?.updated_at, (updated) => {
    if (current() && updated && updated !== current().item.updated_at) soon();
  });
  const poll = setInterval(() => {
    const detail = current();
    const watching = document.visibilityState === 'visible' && ctx.store.get().connection.state === 'open';
    if (detail && watching && !isTerminal(detail.item)) refresh();
  }, POLL_RUNNING_MS);
  return () => { clearTimeout(timer); clearInterval(poll); offFeed(); offSummary(); };
}

// What the detail shows from the store, as values: a refreshed but equal
// status or report list must not redraw the page.
function shownFromStore(state, id, detail) {
  if (typeof id !== 'string') throw new TypeError('an item id names the item');
  const pause = state.status?.pause;
  const reports = (state.reports ?? []).filter((r) => r.item === id);
  const summary = summaryOf(state, id);
  return [
    detail ? state.repos?.find((r) => r.id === detail.item.repo)?.name ?? null : null,
    summary?.switched_to ?? null,
    summary?.pr_url ?? null,
    JSON.stringify(reports),
    state.errors.reports ?? null,
    ...SHARED_LOADS.map((name) => state.errors[name] ?? null),
    Boolean(detail && pause?.repos.includes(detail.item.repo)),
    Boolean(detail && pause?.pipelines.includes(detail.item.pipeline)),
  ];
}

function summaryOf(state, id) {
  return state.summaries?.find((s) => s.id === id) ?? null;
}

function renderDetail(ctx, detail, state, { reload, artifacts }) {
  const item = detail.item;
  const summary = summaryOf(state, item.id);
  const repoName = state.repos?.find((r) => r.id === item.repo)?.name ?? null;
  const look = stateLook({ ...item, waiting_on: detail.waiting_on, waiting_reason: detail.waiting_reason });
  return [
    ...loadErrorNotes(state.errors, SHARED_LOADS, ctx.refresh.one),
    h('div', { class: 'item-hero' },
      chip(look, 'chip-large'),
      h('p', { class: 'item-hero-meta', text: [repoName ?? 'no repository', summary?.switched_to ?? item.pipeline, shortId(item.id)].join(' · ') })),
    outcomeCallout(detail),
    itemActions(ctx, detail, { reload, repoName }),
    section('Details', detailsCard(detail, summary)),
    section('Brief', briefCard(item.brief)),
    section('Stages', stagesTimeline(detail)),
    gatesSection(detail),
    section('Artifacts', artifacts),
    reportsSection(ctx, state, item.id),
  ];
}

function outcomeCallout(detail) {
  const { item, waiting_on: waitingOn, waiting_reason: reason } = detail;
  const state = item.state;
  if (state.state === 'failed') return callout('red', 'Failed', state.error);
  if (state.state === 'succeeded') return callout('green', 'Done', state.summary);
  if (state.state === 'cancelled') return callout('muted', 'Cancelled', state.reason ?? 'Cancelled by the operator.');
  if (state.state === 'blocked') return callout('amber', 'Blocked', state.reason);
  if (waitingOn) {
    const inbox = waitingOn.question ? h('a', { class: 'link-button', href: '#/inbox' }, 'Answer in Inbox') : null;
    return callout('amber', `Waiting on you · ${waitingOn.subject}`, waitingOn.prompt, inbox);
  }
  if (reason && !isTerminal(item)) return callout(waitsOnOperator({ ...item, waiting_reason: reason }) ? 'amber' : 'neutral', 'Waiting', reason.message);
  return null;
}

function callout(tone, title, text, extra = null) {
  return h('div', { class: ['callout', `callout-${tone}`], role: 'note' },
    h('p', { class: 'callout-title', text: title }),
    text ? h('p', { class: 'callout-text', text }) : null, extra);
}

function detailsCard(detail, summary) {
  const item = detail.item;
  const pr = safeHttpUrl(prUrlOf(detail) ?? summary?.pr_url);
  return card(
    row('Priority', priorityLabel(item.priority)),
    row('Review', REVIEW_LABEL[item.autonomy?.review] ?? 'Default'),
    row('Total cost', usd(detail.spent_usd)),
    item.budget_usd ? row('Budget', usd(item.budget_usd)) : null,
    row('Created', dateTime(item.created_at)),
    row('Updated', dateTime(item.updated_at)),
    item.dependencies?.length ? row('Depends on', h('span', { class: 'row-value' },
      item.dependencies.map((dep) => h('a', { class: 'dep-link', href: `#/item/${dep}`, text: shortId(dep) })))) : null,
    pr ? row('Pull request', h('a', { class: 'row-value link', href: pr, target: '_blank', rel: 'noopener noreferrer' },
      'Open', icon('external', { size: 16 }))) : null);
}

function briefCard(brief) {
  const text = brief ?? '';
  const long = text.length > BRIEF_CLAMP_CHARS;
  const body = h('p', { class: 'brief', text: long ? `${text.slice(0, BRIEF_CLAMP_CHARS).trimEnd()}…` : text });
  if (!long) return card(body);
  const toggle = h('button', { class: 'link-button', type: 'button', 'aria-expanded': 'false' }, 'Show full brief');
  toggle.addEventListener('click', () => {
    const open = toggle.getAttribute('aria-expanded') === 'true';
    body.textContent = open ? `${text.slice(0, BRIEF_CLAMP_CHARS).trimEnd()}…` : text;
    toggle.setAttribute('aria-expanded', String(!open));
    toggle.textContent = open ? 'Show full brief' : 'Show less';
  });
  return card(body, toggle);
}

function gatesSection(detail) {
  if (!detail.gates?.length) return null;
  return section('Gates', card(...detail.gates.map((gate) => {
    const decision = gate.decision?.decision ?? (gate.withdrawn ? 'withdrawn' : 'waiting');
    const look = GATE_LOOK[decision] ?? { label: decision, tone: 'muted' };
    return row(`${GATE_SUBJECT[gate.subject] ?? gate.subject} · ${gate.stage_id}`, chip(look));
  })));
}

function reportsSection(ctx, state, itemId) {
  if (state.errors.reports) {
    return section('Reports', card(errorNote(state.errors.reports, () => ctx.refresh.one('reports'))));
  }
  const mine = (state.reports ?? []).filter((r) => r.item === itemId);
  if (mine.length === 0) return null;
  return section('Reports', card(...mine.map((report) => {
    const url = safeHttpUrl(report.location);
    const value = url
      ? h('a', { class: 'row-value link', href: url, target: '_blank', rel: 'noopener noreferrer' }, 'Open', icon('external', { size: 16 }))
      : h('span', { class: 'row-value', text: report.state });
    return row(report.title, value);
  })));
}
