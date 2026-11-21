// The screen's fixed parts: top bar, mode switch, phase panel, status bar, bottom bar.
import { h } from '../../ui/dom.js';
import { icon } from '../../ui/icons.js';
import { segmented } from '../../ui/components.js';
import { dotLook, stagePhase, statusLine } from './state.js';
import { finishBlockedText } from './finish.js';
import { SHORTCUTS } from './keymap.js';
import { openSheet } from '../../ui/sheet.js';

export const MODES = [
  { id: 'interact', label: 'Interact', short: 'Use', icon: 'pointer', key: 'I' },
  { id: 'comment', label: 'Comment', short: 'Comment', icon: 'notes', key: 'C' },
  { id: 'region', label: 'Region', short: 'Region', icon: 'region', key: 'R' },
  { id: 'sketch', label: 'Sketch', short: 'Sketch', icon: 'sketch', key: 'S' },
];

export function deadlineText(deadline, now = Date.now()) {
  const t = Date.parse(deadline);
  if (!Number.isFinite(t)) return '';
  const mins = Math.max(0, Math.round((t - now) / 60000));
  return mins >= 60 ? `Stage ends in ${Math.floor(mins / 60)} h ${mins % 60} min` : `Stage ends in ${mins} min`;
}

export function buildTopBar({ title, onBack, onPreset, onReload, onFinish, onReview, onBackToEditing }) {
  const name = h('h1', { class: 'design-title', tabindex: '-1', text: title });
  const dot = h('span', { class: 'design-dot' });
  const dotText = h('span', { class: 'design-dot-text' });
  const badge = h('span', { class: 'chip tone-amber design-badge', hidden: true }, icon('alert', { size: 14 }), h('span', { class: 'chip-text', text: 'Unsandboxed preview' }));
  const presets = segmented({ label: 'Device size', options: [{ value: 'desktop', label: 'Desktop' }, { value: 'tablet', label: 'Tablet' }, { value: 'phone', label: 'Phone' }], value: 'desktop', onChange: onPreset });
  const reload = h('button', { class: 'icon-button', type: 'button', 'aria-label': 'Reload preview (Shift+R)', onclick: onReload }, icon('reload', { size: 20 }));
  const deadline = h('span', { class: 'design-deadline' });
  const finish = h('button', { class: 'button button-primary button-small', type: 'button', onclick: onFinish }, icon('flag', { size: 16 }), ' Finish');
  const review = h('button', { class: 'button button-primary button-small', type: 'button', hidden: true, onclick: () => onReview() }, icon('flag', { size: 16 }), ' Review');
  const backToEditing = h('button', { class: 'button button-plain button-small', type: 'button', hidden: true, onclick: () => onBackToEditing() }, 'Back to editing');
  const blocked = h('p', { class: 'design-blocked', role: 'status' });
  const el = h('header', { class: 'design-top' },
    h('button', { class: 'nav-back', type: 'button', 'aria-label': 'Back', onclick: onBack }, icon('back', { size: 22 }), h('span', { text: 'Back' })),
    name, h('span', { class: 'design-state' }, dot, dotText), badge, presets, reload, deadline, finish, review, backToEditing, blocked);
  return {
    element: el, title: name,
    update(state, { operator, reviewOpen = false }) {
      const look = dotLook(state);
      dot.className = `design-dot tone-${look.tone}`;
      dotText.textContent = look.label;
      badge.hidden = !(state.browser?.status === 'unsandboxed' || state.session?.sandbox_off);
      for (const b of presets.children) {
        const on = b.dataset.value === state.preset;
        b.setAttribute('aria-checked', String(on));
        b.tabIndex = on ? 0 : -1;
      }
      const live = operator && state.session && (state.session.state === 'live' || state.session.state === 'finishing');
      presets.hidden = reload.hidden = !live;
      deadline.textContent = live ? deadlineText(state.deadline) : '';
      const finishing = state.session?.state === 'finishing';
      finish.hidden = !(operator && state.session && state.session.state === 'live');
      const why = finishBlockedText(state);
      finish.disabled = Boolean(why);
      finish.title = why ?? '';
      blocked.textContent = !finish.hidden && why ? why : '';
      blocked.hidden = !blocked.textContent;
      review.hidden = backToEditing.hidden = !(operator && finishing && !reviewOpen);
    },
  };
}

export function buildModeBar({ onMode, onNewScreen }) {
  const buttons = MODES.map((m) => h('button', { class: 'mode-button', type: 'button', role: 'radio', 'aria-checked': 'false', dataset: { mode: m.id }, title: `${m.label} (${m.key})`, onclick: () => onMode(m.id) },
    icon(m.icon, { size: 18 }), h('span', { text: m.label })));
  const newScreen = h('button', { class: 'button button-plain button-small', type: 'button', onclick: onNewScreen }, icon('plus', { size: 16 }), ' New screen');
  const el = h('div', { class: 'design-modes', role: 'radiogroup', 'aria-label': 'Mode' }, buttons, newScreen);
  return {
    element: el,
    update(mode, enabled) {
      for (const b of buttons) { const on = b.dataset.mode === mode; b.setAttribute('aria-checked', String(on)); b.disabled = !enabled; }
      newScreen.hidden = !enabled;
    },
  };
}

export function buildBottomBar({ onMode, onNotes }) {
  const mk = (id, label, ic, fn) => h('button', { class: 'bottom-button', type: 'button', dataset: { mode: id }, 'aria-pressed': 'false', onclick: fn }, icon(ic, { size: 22 }), h('span', { text: label }));
  const buttons = [
    mk('interact', 'Use', 'pointer', () => onMode('interact')), mk('comment', 'Comment', 'notes', () => onMode('comment')),
    mk('region', 'Region', 'region', () => onMode('region')), mk('notes', 'Notes', 'notes', onNotes),
  ];
  return {
    element: h('nav', { class: 'design-bottombar', 'aria-label': 'Design modes' }, buttons),
    update(mode) { for (const b of buttons) if (b.dataset.mode !== 'notes') b.setAttribute('aria-pressed', String(b.dataset.mode === mode)); },
  };
}

// The text and actions over the stage for every state that is not a plain live frame.
export function buildPhasePanel(actions) {
  const title = h('h2', { class: 'phase-title' });
  const text = h('p', { class: 'phase-text' });
  const buttons = h('div', { class: 'phase-actions' });
  const skeleton = h('div', { class: 'phase-skeleton', 'aria-hidden': 'true' }, h('span', { class: 'skeleton-bar' }), h('span', { class: 'skeleton-bar short' }), h('span', { class: 'skeleton-block' }));
  const el = h('div', { class: 'design-phase', role: 'status' }, skeleton, title, text, buttons);
  const btn = (label, fn, primary = false) => h('button', { class: ['button', primary ? 'button-primary' : 'button-plain'], type: 'button', onclick: fn }, label);
  let last = '';
  return {
    element: el,
    update(state, { operator, features, continued }) {
      const phase = stagePhase(state, { operator, features });
      const overlay = !(phase.kind === 'live' || phase.kind === 'finishing') && !(continued && (phase.kind === 'no_browser' || phase.kind === 'sandbox'));
      el.hidden = !overlay;
      el.dataset.kind = phase.kind;
      el.className = `design-phase tone-${phase.tone}`;
      skeleton.hidden = !(phase.kind === 'starting' || phase.kind === 'loading');
      title.textContent = phase.title;
      text.textContent = phase.text;
      const sig = `${phase.kind}|${state.item}`;
      if (sig === last) return phase;
      last = sig;
      const list = [];
      if (phase.kind === 'starting') list.push(btn('Cancel session', actions.cancelSession));
      if (phase.kind === 'preview_down') list.push(btn('Reload', actions.reload, true), btn('Tell agent again', actions.tellAgent));
      if (phase.kind === 'no_browser' || phase.kind === 'sandbox') list.push(btn('Continue without preview', actions.continueWithout, true));
      if (phase.kind === 'ended') list.push(h('a', { class: 'button button-plain', href: `#/item/${state.item}` }, 'View the ledger'));
      if (phase.kind === 'none' || phase.kind === 'observer' || phase.kind === 'unavailable') list.push(h('a', { class: 'button button-plain', href: `#/item/${state.item}` }, 'Open the item'));
      buttons.replaceChildren(...list);
      return phase;
    },
  };
}

export function buildStatusBar() {
  const text = h('p', { class: 'design-status-text' });
  const live = h('p', { class: 'sr-only', role: 'status', 'aria-live': 'polite' });
  const frameText = h('p', { class: 'design-frame-text' });
  const hosts = h('details', { class: 'design-hosts', hidden: true }, h('summary', { text: '' }), h('ul', { class: 'plain-list' }));
  const el = h('footer', { class: 'design-status' }, frameText, text, hosts, live);
  let lastLive = '';
  return {
    element: el, frameText,
    update(state, { path }) {
      const sha = state.previewCommit ?? state.session?.finish_head;
      const commit = sha ? `preview at ${sha.slice(0, 7)}` : '';
      const line = statusLine(state);
      text.textContent = [commit, line].filter(Boolean).join(' · ');
      const sel = state.selection?.label;
      frameText.textContent = `Preview of ${path || 'the app'}${sel ? `, selected: ${sel}` : ''}`;
      hosts.hidden = state.blocked.length === 0;
      if (state.blocked.length) {
        hosts.querySelector('summary').textContent = `${state.blocked.length} blocked host${state.blocked.length === 1 ? '' : 's'}`;
        hosts.querySelector('ul').replaceChildren(...state.blocked.map((x) => h('li', { text: x })));
      }
      // Polite announcements: the state line, only when it changes.
      const announce = line || stagePhase(state).title;
      if (announce !== lastLive) { lastLive = announce; live.textContent = announce; }
    },
  };
}

export function openShortcuts() {
  const rows = SHORTCUTS.map(([k, d]) => h('tr', {}, h('th', { scope: 'row' }, h('kbd', { text: k })), h('td', { text: d })));
  openSheet({ title: 'Keyboard shortcuts', content: h('table', { class: 'shortcuts' }, h('caption', { class: 'sr-only', text: 'Shortcuts, active when focus is not in a text field' }), h('tbody', {}, rows)) });
}
