// One open question: its options as buttons, a free-text answer, and the
// card folding away once the daemon has recorded the answer.
import { h, append } from '../../ui/dom.js';
import { icon } from '../../ui/icons.js';
import { collapseOut } from '../../ui/motion.js';
import { describeError } from '../../ui/components.js';
import { age } from '../../core/format.js';

export function questionCard(ctx, question, itemTitle) {
  const error = h('p', { class: 'form-error', role: 'alert' });
  const input = h('input', { class: 'input input-inline', type: 'text', name: 'answer', placeholder: 'Write an answer', autocomplete: 'off', enterkeyhint: 'send', 'aria-label': `Answer Q${question.number}` });
  const send = h('button', { class: 'icon-button icon-button-filled', type: 'submit', 'aria-label': 'Send answer' }, icon('send', { size: 20 }));
  const card = h('li', { class: 'question-card' });
  const answer = (text) => submitAnswer({ ctx, question, text, card, error });
  const form = h('form', { class: 'answer-form', novalidate: true }, input, send);
  form.addEventListener('submit', (event) => {
    event.preventDefault();
    if (input.value.trim()) answer(input.value.trim());
    else error.textContent = 'Write an answer first, or pick an option.';
  });
  append(card, [
    h('div', { class: 'question-head' },
      h('span', { class: 'question-number', text: `Q${question.number}` }),
      h('span', { class: 'question-age', text: age(question.asked_at) })),
    h('p', { class: 'question-text', text: question.text }),
    question.item ? h('a', { class: 'question-item', href: `#/item/${question.item}`, text: itemTitle ?? 'Open the item' }) : null,
    question.options.length ? h('div', { class: 'question-options', role: 'group', 'aria-label': 'Options' },
      question.options.map((option) => h('button', { class: 'option-button', type: 'button', onclick: () => answer(option) }, option))) : null,
    form, error]);
  return card;
}

async function submitAnswer({ ctx, question, text, card, error }) {
  if (card.getAttribute('aria-busy') === 'true') return;
  card.setAttribute('aria-busy', 'true');
  error.textContent = '';
  for (const control of card.querySelectorAll('button, input')) control.disabled = true;
  try {
    const answered = await ctx.cmds.answer(question.id, text);
    card.classList.add('is-answered');
    await collapseOut(card);
    const questions = ctx.store.get().questions ?? [];
    ctx.store.set({ questions: questions.map((q) => (q.id === answered.id ? answered : q)) });
  } catch (err) {
    error.textContent = describeError(err);
    card.removeAttribute('aria-busy');
    for (const control of card.querySelectorAll('button, input')) control.disabled = false;
  }
}
