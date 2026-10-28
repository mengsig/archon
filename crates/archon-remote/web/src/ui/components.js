// Small building blocks shared by the screens.
import { h } from './dom.js';
import { icon } from './icons.js';

export function chip(look, extraClass = '') {
  return h('span', { class: ['chip', `tone-${look.tone}`, look.live && 'is-live', extraClass] }, look.label);
}

export function screenHeader({ title, back = null, actions = [], subtitle = null }) {
  const lead = back
    ? h('button', { class: 'nav-back', type: 'button', 'aria-label': back.label, onclick: back.onClick },
      icon('back', { size: 22 }), h('span', { text: back.text }))
    : null;
  return h('header', { class: ['screen-header', back ? 'has-back' : 'is-large'] },
    h('div', { class: 'screen-header-bar' }, lead, h('div', { class: 'screen-header-actions' }, actions)),
    h('h1', { class: 'screen-title', text: title, tabindex: '-1' }),
    subtitle ? h('p', { class: 'screen-subtitle', text: subtitle }) : null);
}

export function iconButton({ name, label, onClick, tone = '' }) {
  return h('button', { class: ['icon-button', tone], type: 'button', 'aria-label': label, onclick: onClick }, icon(name, { size: 22 }));
}

export function section(title, ...children) {
  return h('section', { class: 'section', 'aria-label': title },
    h('h2', { class: 'section-title', text: title }), ...children);
}

export function card(...children) {
  return h('div', { class: 'card' }, ...children);
}

// A label and value on one line, as in iOS settings lists.
export function row(label, value, { tone = null } = {}) {
  return h('div', { class: 'row' },
    h('span', { class: 'row-label', text: label }),
    value instanceof Node ? value : h('span', { class: ['row-value', tone && `tone-text-${tone}`], text: value ?? '—' }));
}

export function emptyState({ title, text, iconName = null }) {
  return h('div', { class: 'empty', role: 'status' },
    iconName ? h('div', { class: 'empty-icon' }, icon(iconName, { size: 30 })) : null,
    h('p', { class: 'empty-title', text: title }),
    text ? h('p', { class: 'empty-text', text }) : null);
}

export function errorNote(error, retry = null, what = null) {
  const text = what ? `${what}: ${describeError(error)}` : describeError(error);
  return h('div', { class: 'error-note', role: 'alert' },
    h('span', { text }),
    retry ? h('button', { class: 'link-button', type: 'button', onclick: retry }, 'Try again') : null);
}

const LOAD_LABEL = { summaries: 'items', repos: 'repositories', status: 'the status', questions: 'questions' };

// A note naming each shared load in `names` that failed, each with its own
// retry, for screens that show those loads beside their own data.
export function loadErrorNotes(errors, names, retry) {
  if (!errors || typeof retry !== 'function') throw new TypeError('loadErrorNotes needs the errors and a retry');
  const unknown = names.filter((name) => !(name in LOAD_LABEL));
  if (unknown.length) throw new Error(`no label for the ${unknown.join(', ')} load`);
  return names.filter((name) => errors[name])
    .map((name) => errorNote(errors[name], () => retry(name), `Could not load ${LOAD_LABEL[name]}`));
}

export function describeError(error) {
  if (!error) return 'Something went wrong.';
  if (error.code === 'disconnected' || error.code === 'timeout') return `${error.message}. It will retry once connected.`;
  return error.message || String(error);
}

export function spinner(label = 'Loading') {
  return h('div', { class: 'spinner', role: 'status', 'aria-label': label });
}

export function skeletonRows(count = 3) {
  return h('div', { class: 'skeleton-list', 'aria-hidden': 'true' },
    Array.from({ length: count }, () => h('div', { class: 'skeleton-row' })));
}

// A row of mutually exclusive buttons, like a UISegmentedControl.
export function segmented({ label, options, value, onChange }) {
  const group = h('div', { class: 'segmented', role: 'radiogroup', 'aria-label': label });
  const select = (next) => {
    for (const button of group.children) {
      const on = button.dataset.value === next;
      button.setAttribute('aria-checked', String(on));
      button.tabIndex = on ? 0 : -1;
    }
    onChange(next);
  };
  for (const option of options) {
    group.append(h('button', {
      class: 'segment', type: 'button', role: 'radio', dataset: { value: option.value },
      'aria-checked': String(option.value === value), tabindex: option.value === value ? '0' : '-1',
      onclick: () => select(option.value),
      onkeydown: (event) => arrowSelect(event, group, select),
    }, option.label));
  }
  return group;
}

function arrowSelect(event, group, select) {
  const step = { ArrowRight: 1, ArrowDown: 1, ArrowLeft: -1, ArrowUp: -1 }[event.key];
  if (!step) return;
  event.preventDefault();
  const buttons = [...group.children];
  const at = buttons.indexOf(event.currentTarget);
  const next = buttons[(at + step + buttons.length) % buttons.length];
  next.focus();
  select(next.dataset.value);
}

// An on/off switch. onChange returns a promise; while it runs the switch is
// disabled, and when it fails the switch flips back and onError shows why.
export function toggle({ label, checked, onChange, onError, description = null }) {
  if (typeof onError !== 'function') throw new TypeError('a toggle needs onError to show failures');
  const input = h('input', { class: 'switch-input', type: 'checkbox', role: 'switch', checked, 'aria-label': label });
  input.addEventListener('change', async () => {
    const wanted = input.checked;
    input.disabled = true;
    try {
      await onChange(wanted);
    } catch (err) {
      input.checked = !wanted;
      onError(err);
    } finally {
      input.disabled = false;
    }
  });
  return h('label', { class: 'switch-row' },
    h('span', { class: 'switch-text' }, h('span', { class: 'row-label', text: label }),
      description ? h('span', { class: 'row-hint', text: description }) : null),
    h('span', { class: 'switch' }, input, h('span', { class: 'switch-track', 'aria-hidden': 'true' })));
}

export function field({ label, control, hint = null, error = null }) {
  const id = control.id || `field-${Math.random().toString(36).slice(2, 9)}`;
  control.id = id;
  return h('div', { class: 'field' },
    h('label', { class: 'field-label', for: id, text: label }),
    control,
    hint ? h('p', { class: 'field-hint', text: hint }) : null,
    error);
}
