// What the operator can do to an item from here: cancel, retry, and pause
// or resume its repository or pipeline.
import { h } from '../../ui/dom.js';
import { icon } from '../../ui/icons.js';
import { confirmSheet } from '../../ui/sheet.js';
import { toast } from '../../ui/toast.js';
import { describeError } from '../../ui/components.js';
import { isTerminal, stateKind } from '../../core/format.js';

const RETRYABLE = ['failed', 'cancelled', 'blocked'];

export function itemActions(ctx, detail, { reload, repoName }) {
  const item = detail.item;
  const pause = ctx.store.get().status?.pause ?? { repos: [], pipelines: [] };
  const buttons = [];
  if (!isTerminal(item)) buttons.push(action('stop', 'Cancel', 'red', () => cancel(ctx, item, reload)));
  if (RETRYABLE.includes(stateKind(item))) buttons.push(action('retry', 'Retry', 'accent', () => retry(ctx, item, reload)));
  if (item.repo) {
    const paused = pause.repos.includes(item.repo);
    buttons.push(pauseAction(ctx, { scope: 'repo', name: item.repo }, paused, repoName ?? 'repository', reload));
  }
  const pipelinePaused = pause.pipelines.includes(item.pipeline);
  buttons.push(pauseAction(ctx, { scope: 'pipeline', name: item.pipeline }, pipelinePaused, item.pipeline, reload));
  return h('div', { class: 'action-bar', role: 'group', 'aria-label': 'Item actions' }, buttons);
}

function action(iconName, label, tone, onClick, ariaLabel = label) {
  return h('button', { class: ['action', `tone-${tone}`], type: 'button', 'aria-label': ariaLabel, onclick: onClick },
    h('span', { class: 'action-icon' }, icon(iconName, { size: 20 })), h('span', { class: 'action-label', text: label }));
}

function pauseAction(ctx, target, paused, name, reload) {
  const what = target.scope === 'repo' ? 'repo' : 'pipeline';
  const label = `${paused ? 'Resume' : 'Pause'} ${what}`;
  return action(paused ? 'play' : 'pause', label, paused ? 'green' : 'neutral',
    () => togglePause(ctx, target, paused, name, reload), `${label} ${name}`);
}

async function cancel(ctx, item, reload) {
  const sure = await confirmSheet({
    title: 'Cancel this item?',
    message: `Archon stops “${item.title}” and its agents. You can retry it later.`,
    confirmLabel: 'Cancel item',
  });
  if (!sure) return;
  await run(ctx, () => ctx.cmds.cancel(item.id), 'Item cancelled', reload);
}

async function retry(ctx, item, reload) {
  const sure = await confirmSheet({
    title: 'Retry this item?',
    message: `“${item.title}” goes back in the queue and runs again.`,
    confirmLabel: 'Retry', tone: 'primary', cancelLabel: 'Not now',
  });
  if (!sure) return;
  await run(ctx, () => ctx.cmds.retry(item.id), 'Back in the queue', reload);
}

async function togglePause(ctx, target, paused, name, reload) {
  const done = paused ? `Resumed ${name}` : `Paused ${name}`;
  await run(ctx, () => (paused ? ctx.cmds.resume(target) : ctx.cmds.pause(target)), done, reload);
}

async function run(ctx, change, message, reload) {
  try {
    await change();
    toast(message, { tone: 'green' });
  } catch (err) {
    toast(describeError(err), { tone: 'red' });
  }
  await ctx.refresh.many(['summaries', 'status']);
  await reload();
}
