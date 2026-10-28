// The repositories and pipelines Archon knows, each pausable on its own.
import { h } from '../../ui/dom.js';
import { card, chip, emptyState, errorNote, spinner, describeError } from '../../ui/components.js';
import { toast } from '../../ui/toast.js';

export function reposCard(ctx, repos, pause) {
  if (repos === null) return card(spinner('Loading repositories'));
  if (repos.length === 0) return card(emptyState({ title: 'No repositories', text: 'Register one on the host to queue work against it.' }));
  return card(...repos.map((repo) => {
    const paused = pause.repos.includes(repo.id);
    return catalogRow({
      title: repo.name,
      text: [repo.ship === 'local' ? 'Ships locally' : 'Opens pull requests', repo.base_branch, repo.default_pipeline].filter(Boolean).join(' · '),
      paused,
      onToggle: () => togglePause(ctx, { scope: 'repo', name: repo.id }, paused, repo.name),
    });
  }));
}

export function pipelinesCard(pipelines, pause, retry) {
  if (pipelines.error) return card(errorNote(pipelines.error, retry));
  if (!pipelines.data) return card(spinner('Loading pipelines'));
  const list = pipelines.data.pipelines ?? [];
  return card(...list.map((p) => catalogRow({
    title: p.name,
    text: `${p.stages.length} stage${p.stages.length === 1 ? '' : 's'} · ${p.description}`,
    paused: pause.pipelines.includes(p.name),
  })), ...(pipelines.data.problems ?? []).map((problem) => h('p', { class: 'card-foot tone-text-amber', text: describeProblem(problem) })));
}

function describeProblem(problem) {
  return `${problem.name}: ${problem.error}`;
}

function catalogRow({ title, text, paused, onToggle = null }) {
  return h('div', { class: 'catalog-row' },
    h('div', { class: 'catalog-text' },
      h('span', { class: 'catalog-title' }, title, paused ? chip({ label: 'Paused', tone: 'amber' }) : null),
      h('span', { class: 'catalog-sub', text })),
    onToggle ? h('button', { class: 'button button-small button-plain', type: 'button', onclick: onToggle, 'aria-label': `${paused ? 'Resume' : 'Pause'} ${title}` }, paused ? 'Resume' : 'Pause') : null);
}

async function togglePause(ctx, target, paused, name) {
  try {
    await (paused ? ctx.cmds.resume(target) : ctx.cmds.pause(target));
    toast(paused ? `Resumed ${name}` : `Paused ${name}`, { tone: 'green' });
  } catch (err) {
    toast(describeError(err), { tone: 'red' });
  }
  await ctx.refresh.one('status');
}
