import { h } from '../../ui/dom.js';
import { openSheet } from '../../ui/sheet.js';
import { field, describeError } from '../../ui/components.js';
import { assertOperator } from '../../core/role.js';

const COMMANDS = [
  ['/status', 'Workspace status'], ['/questions', 'Open questions'],
  ['/approve', 'Approve a pending gate'], ['/reject', 'Reject a pending gate'],
  ['/queue', 'Queue and states'], ['/repos', 'Registered repositories'],
  ['/lessons', 'Proposed lessons'], ['/archon ', 'Settings and operator controls'],
];

export function openCommands(ctx, initial = '') {
  assertOperator(ctx, 'Commands');
  const input = h('input', { class: 'input', type: 'text', value: initial, placeholder: '/status', autocomplete: 'off', maxlength: 4000 });
  const result = h('div', { class: 'command-result', role: 'status', 'aria-live': 'polite' });
  const output = h('ol', { class: 'command-output', 'aria-label': 'Command outcomes' });
  const submit = h('button', { class: 'button button-primary', type: 'submit', text: 'Send command' });
  const form = h('form', { class: 'command-form', 'aria-label': 'Operator commands' },
    h('p', { class: 'section-foot', text: 'Use the same operator commands as in the terminal. Changes use your paired device’s role and the host’s confirmation rules.' }),
    field({ label: 'Operator command', control: input }), submit, result,
    h('div', { class: 'command-presets' }, COMMANDS.map(([command, label]) => h('button', {
      class: 'button button-plain button-small', type: 'button', onclick: () => { input.value = command; input.focus(); },
    }, label))),
    h('a', { class: 'link-button', href: '#/inbox', onclick: () => sheet.close() }, 'Open questions and confirmations'), output);
  const start = ctx.store.get().feed.at(-1)?.seq ?? 0;
  const off = ctx.store.watch((state) => state.feed, (entries) => {
    const lines = entries.filter((entry) => entry.seq > start && entry.entry.entry !== 'operator')
      .map((entry) => entry.entry.text ?? entry.entry.message).filter((text) => typeof text === 'string');
    output.replaceChildren(...lines.slice(-20).map((text) => h('li', { text })));
  });
  const sheet = openSheet({ title: 'Operator commands', content: form, onClose: off });
  form.addEventListener('submit', async (event) => {
    event.preventDefault();
    const text = input.value.trim();
    if (!text.startsWith('/') && !text.startsWith('archon ')) { result.textContent = 'Start with / or archon, followed by an operator command.'; return; }
    if (text.length > 4000 || submit.disabled) return;
    submit.disabled = true;
    try {
      await ctx.cmds.operatorCommand(text);
      result.textContent = 'Command received. The host’s outcome appears below; any required confirmation is in Inbox.';
      ctx.refresh.many(['status', 'summaries', 'questions', 'repos']);
    } catch (error) { result.textContent = describeError(error); }
    finally { submit.disabled = false; }
  });
  return sheet;
}
