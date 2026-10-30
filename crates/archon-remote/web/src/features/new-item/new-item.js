// The new-item sheet: repository, title, brief, pipeline, priority and
// review level, sent as one enqueue; the daemon's refusals show inline.
import { h } from '../../ui/dom.js';
import { openSheet } from '../../ui/sheet.js';
import { toast } from '../../ui/toast.js';
import { field, segmented, describeError, errorNote, loadErrorNotes } from '../../ui/components.js';

const PRIORITIES = [
  { value: 'low', label: 'Low' }, { value: 'normal', label: 'Normal' },
  { value: 'high', label: 'High' }, { value: 'critical', label: 'Critical' },
];
const REVIEWS = [
  { value: '', label: 'Default' }, { value: 'strict', label: 'Strict' },
  { value: 'standard', label: 'Standard' }, { value: 'none', label: 'None' },
];

export function openNewItem(ctx) {
  const draft = { repo: '', pipeline: '', priority: 'normal', review: '', idempotencyKey: newKey() };
  const controls = buildControls();
  const repoStatus = h('div', { class: 'list-status' });
  const error = h('p', { class: 'form-error', role: 'alert', id: 'new-item-error' });
  const submit = h('button', { class: 'button button-primary button-block', type: 'submit' }, 'Queue item');
  const form = h('form', { class: 'sheet-form', novalidate: true, 'aria-label': 'New item' },
    field({ label: 'Repository', control: controls.repo }), repoStatus,
    field({ label: 'Title', control: controls.title }),
    field({ label: 'Brief', control: controls.brief, hint: 'What should be done, and how you will know it is right.' }),
    field({ label: 'Pipeline', control: controls.pipeline, error: controls.pipelineHint }), controls.pipelineStatus,
    h('div', { class: 'field' }, h('span', { class: 'field-label', text: 'Priority' }),
      segmented({ label: 'Priority', options: PRIORITIES, value: draft.priority, onChange: (v) => { draft.priority = v; } })),
    h('div', { class: 'field' }, h('span', { class: 'field-label', text: 'Review' }),
      segmented({ label: 'Review level', options: REVIEWS, value: draft.review, onChange: (v) => { draft.review = v; } })),
    error, submit);

  let offRepos = () => {};
  const sheet = openSheet({ title: 'New item', content: form, size: 'tall', onClose: () => offRepos() });
  const loadPipelines = pipelineLoader(ctx, controls);
  offRepos = followRepos(ctx, { controls, draft, repoStatus, loadPipelines });
  controls.pipeline.addEventListener('change', () => { draft.pipeline = controls.pipeline.value; controls.describe(); });
  form.addEventListener('submit', (event) => {
    event.preventDefault();
    submitItem({ ctx, draft, controls, error, submit, sheet });
  });
  const { repos, errors } = ctx.store.get();
  if (repos === null || errors.repos) ctx.refresh.one('repos');
}

// The repository list follows the store while the sheet is open: a load that
// fails shows with a retry, and one that arrives late fills the choice in.
function followRepos(ctx, { controls, draft, repoStatus, loadPipelines }) {
  if (typeof loadPipelines !== 'function') throw new TypeError('followRepos needs the pipeline loader');
  let picked = false;
  let pipelinesFor;
  controls.repo.addEventListener('change', () => { picked = true; show(); });
  const show = () => {
    const { repos, errors } = ctx.store.get();
    const chosen = picked ? controls.repo.value : (repos?.[0]?.name ?? '');
    controls.setRepos(repos ?? [], chosen);
    draft.repo = controls.repo.value;
    if (!picked && chosen && draft.repo !== chosen) throw new Error(`the repository ${chosen} was not offered`);
    repoStatus.replaceChildren(...loadErrorNotes(errors, ['repos'], ctx.refresh.one));
    if (!errors.repos && repos === null) repoStatus.append(h('p', { class: 'field-hint', text: 'Listing repositories…' }));
    if (!errors.repos && repos?.length === 0) repoStatus.append(h('p', { class: 'field-hint', text: 'No repository is registered yet. Register one on the host, or pick a pipeline that needs none.' }));
    if (draft.repo === pipelinesFor) return;
    pipelinesFor = draft.repo;
    loadPipelines(draft.repo);
  };
  return ctx.store.watch((s) => [s.repos, s.errors.repos], show);
}

function buildControls() {
  const repo = h('select', { class: 'input', name: 'repo' });
  const title = h('input', { class: 'input', name: 'title', type: 'text', autocomplete: 'off', placeholder: 'One line: what to do', autofocus: true, enterkeyhint: 'next' });
  const brief = h('textarea', { class: 'input textarea', name: 'brief', rows: '5', placeholder: 'Context, constraints, acceptance criteria' });
  const pipeline = h('select', { class: 'input', name: 'pipeline' }, h('option', { value: '', text: 'Repository default' }));
  const pipelineHint = h('p', { class: 'field-hint' });
  const pipelineStatus = h('div', { class: 'list-status' });
  let catalog = [];
  return {
    repo, title, brief, pipeline, pipelineHint, pipelineStatus,
    setRepos(repos, chosen) {
      repo.replaceChildren(...repos.map((r) => h('option', { value: r.name, text: r.name, selected: r.name === chosen })),
        h('option', { value: '', text: 'No repository', selected: chosen === '' }));
    },
    setCatalog(pipelines) {
      catalog = pipelines;
      const chosen = pipeline.value;
      pipeline.replaceChildren(h('option', { value: '', text: 'Repository default' }),
        ...pipelines.map((p) => h('option', { value: p.name, text: p.name, selected: p.name === chosen })));
      this.describe();
    },
    describe() {
      pipelineHint.textContent = catalog.find((p) => p.name === pipeline.value)?.description ?? 'The repository’s default pipeline.';
    },
  };
}

// Only the latest repository's listing lands; a failed one shows with a retry.
function pipelineLoader(ctx, controls) {
  let latest = 0;
  const load = async (repo) => {
    const ticket = ++latest;
    controls.pipeline.disabled = true;
    controls.pipelineStatus.replaceChildren();
    try {
      const catalog = await ctx.cmds.pipelines(repo || null);
      if (ticket === latest) controls.setCatalog(catalog.pipelines ?? []);
    } catch (err) {
      if (ticket === latest) controls.pipelineStatus.replaceChildren(errorNote(err, () => load(repo), 'Could not list pipelines'));
    } finally {
      if (ticket === latest) controls.pipeline.disabled = false;
    }
  };
  return load;
}

async function submitItem({ ctx, draft, controls, error, submit, sheet }) {
  const title = controls.title.value.trim();
  const brief = controls.brief.value.trim();
  if (!title) return invalid(error, controls.title, 'Give the item a title.');
  if (!brief) return invalid(error, controls.brief, 'Write a brief, so the agents know what to do.');
  submit.disabled = true;
  submit.textContent = 'Queueing…';
  error.textContent = '';
  try {
    const item = await ctx.cmds.enqueue({ ...draft, repo: controls.repo.value, pipeline: controls.pipeline.value, title, brief });
    await sheet.close();
    toast(`Queued “${item.title}”`, { tone: 'green' });
    ctx.refresh.one('summaries');
    ctx.router.go('#/queue');
  } catch (err) {
    error.textContent = describeError(err);
    // A lost answer may have queued it: resending the same key returns that item.
    if (err.code !== 'timeout' && err.code !== 'disconnected') draft.idempotencyKey = newKey();
    submit.disabled = false;
    submit.textContent = 'Queue item';
  }
}

function invalid(error, control, message) {
  error.textContent = message;
  control.setAttribute('aria-invalid', 'true');
  control.focus();
  control.addEventListener('input', () => control.removeAttribute('aria-invalid'), { once: true });
}

// getRandomValues works outside secure contexts too, unlike randomUUID.
function newKey() {
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  return `web-${Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('')}`;
}
