// The new-item sheet: repository, title, brief, pipeline, priority and
// review level, sent as one enqueue; the daemon's refusals show inline.
import { h } from '../../ui/dom.js';
import { openSheet } from '../../ui/sheet.js';
import { toast } from '../../ui/toast.js';
import { field, segmented, describeError } from '../../ui/components.js';

const PRIORITIES = [
  { value: 'low', label: 'Low' }, { value: 'normal', label: 'Normal' },
  { value: 'high', label: 'High' }, { value: 'critical', label: 'Critical' },
];
const REVIEWS = [
  { value: '', label: 'Default' }, { value: 'strict', label: 'Strict' },
  { value: 'standard', label: 'Standard' }, { value: 'none', label: 'None' },
];

export function openNewItem(ctx) {
  const repos = ctx.store.get().repos ?? [];
  const draft = { repo: repos[0]?.name ?? '', pipeline: '', priority: 'normal', review: '', idempotencyKey: newKey() };
  const controls = buildControls(repos, draft);
  const error = h('p', { class: 'form-error', role: 'alert', id: 'new-item-error' });
  const submit = h('button', { class: 'button button-primary button-block', type: 'submit' }, 'Queue item');
  const form = h('form', { class: 'sheet-form', novalidate: true, 'aria-label': 'New item' },
    field({ label: 'Repository', control: controls.repo }),
    field({ label: 'Title', control: controls.title }),
    field({ label: 'Brief', control: controls.brief, hint: 'What should be done, and how you will know it is right.' }),
    field({ label: 'Pipeline', control: controls.pipeline, error: controls.pipelineHint }),
    h('div', { class: 'field' }, h('span', { class: 'field-label', text: 'Priority' }),
      segmented({ label: 'Priority', options: PRIORITIES, value: draft.priority, onChange: (v) => { draft.priority = v; } })),
    h('div', { class: 'field' }, h('span', { class: 'field-label', text: 'Review' }),
      segmented({ label: 'Review level', options: REVIEWS, value: draft.review, onChange: (v) => { draft.review = v; } })),
    error, submit);

  const sheet = openSheet({ title: 'New item', content: form, size: 'tall' });
  const loadPipelines = pipelineLoader(ctx, controls, error);
  controls.repo.addEventListener('change', () => { draft.repo = controls.repo.value; loadPipelines(draft.repo); });
  controls.pipeline.addEventListener('change', () => { draft.pipeline = controls.pipeline.value; controls.describe(); });
  form.addEventListener('submit', (event) => {
    event.preventDefault();
    submitItem({ ctx, draft, controls, error, submit, sheet });
  });
  loadPipelines(draft.repo);
  if (repos.length === 0) error.textContent = 'No repository is registered yet. Register one on the host, or pick a pipeline that needs none.';
}

function buildControls(repos, draft) {
  const repo = h('select', { class: 'input', name: 'repo' },
    repos.map((r) => h('option', { value: r.name, text: r.name, selected: r.name === draft.repo })),
    h('option', { value: '', text: 'No repository', selected: repos.length === 0 }));
  const title = h('input', { class: 'input', name: 'title', type: 'text', autocomplete: 'off', placeholder: 'One line: what to do', autofocus: true, enterkeyhint: 'next' });
  const brief = h('textarea', { class: 'input textarea', name: 'brief', rows: '5', placeholder: 'Context, constraints, acceptance criteria' });
  const pipeline = h('select', { class: 'input', name: 'pipeline' }, h('option', { value: '', text: 'Repository default' }));
  const pipelineHint = h('p', { class: 'field-hint' });
  let catalog = [];
  return {
    repo, title, brief, pipeline, pipelineHint,
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

function pipelineLoader(ctx, controls, error) {
  let latest = 0;
  return async (repo) => {
    const ticket = ++latest;
    controls.pipeline.disabled = true;
    try {
      const catalog = await ctx.cmds.pipelines(repo || null);
      if (ticket === latest) controls.setCatalog(catalog.pipelines ?? []);
    } catch (err) {
      if (ticket === latest) error.textContent = `Pipelines could not be listed: ${describeError(err)}`;
    } finally {
      if (ticket === latest) controls.pipeline.disabled = false;
    }
  };
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
