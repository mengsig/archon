// The paired app's frame: connection banner, the current view with its
// transition, and the tab bar with the inbox badge.
import { h } from './ui/dom.js';
import { icon } from './ui/icons.js';
import { createRouter } from './core/router.js';
import { mountQueue } from './features/queue/queue.js';
import { mountItem } from './features/item/item.js';
import { mountInbox } from './features/inbox/inbox.js';
import { mountChat } from './features/chat/chat.js';
import { mountOverview } from './features/overview/overview.js';
import { mountSettings } from './features/settings/settings.js';

const SCREENS = { queue: mountQueue, item: mountItem, inbox: mountInbox, chat: mountChat, overview: mountOverview, settings: mountSettings };
const TABS = [
  { id: 'queue', label: 'Queue', icon: 'queue' },
  { id: 'inbox', label: 'Inbox', icon: 'inbox' },
  { id: 'chat', label: 'Chat', icon: 'chat' },
  { id: 'overview', label: 'Overview', icon: 'overview' },
];

export function mountShell(root, app, { onForget }) {
  const banner = h('div', { class: 'conn-banner', role: 'status', 'aria-live': 'polite' });
  const views = h('main', { class: 'views', id: 'views' });
  const tabbar = buildTabbar();
  const shell = h('div', { class: 'shell', id: 'shell' }, banner, views, tabbar.nav);
  root.replaceChildren(shell, h('div', { id: 'overlay', class: 'overlay' }), h('div', { id: 'toasts', class: 'toasts', 'aria-live': 'polite' }));

  const scrollMemory = new Map();
  let current = null;
  const router = createRouter((route, transition) => {
    if (current) {
      scrollMemory.set(current.key, window.scrollY);
      current.unmount?.();
      current.view.remove();
    }
    const view = h('section', { class: ['view', `view-${route.name}`, transition !== 'none' && `enter-${transition}`] });
    views.append(view);
    shell.dataset.route = route.name;
    const ctx = { ...app, router, route, onForget };
    const key = `${route.name}:${route.param ?? ''}`;
    current = { key, view, unmount: SCREENS[route.name](view, ctx) };
    tabbar.select(route.tab);
    window.scrollTo(0, transition === 'pop' ? (scrollMemory.get(key) ?? 0) : 0);
    if (transition !== 'none') view.querySelector('.screen-title')?.focus({ preventScroll: true });
  });

  const offs = [
    app.store.watch((s) => s.connection, (connection) => renderBanner(banner, connection, app)),
    app.store.watch((s) => s.questions, (questions) => tabbar.badge(openCount(questions))),
  ];
  router.start();
  return () => {
    current?.unmount?.();
    for (const off of offs) off();
  };
}

function buildTabbar() {
  const buttons = new Map();
  const badge = h('span', { class: 'tab-badge', 'aria-hidden': 'true', hidden: true });
  const nav = h('nav', { class: 'tabbar', 'aria-label': 'Sections' },
    TABS.map((tab) => {
      const link = h('a', { class: 'tab', href: `#/${tab.id}`, dataset: { tab: tab.id } },
        h('span', { class: 'tab-icon' }, icon(tab.icon, { size: 24 }), tab.id === 'inbox' ? badge : null),
        h('span', { class: 'tab-label', text: tab.label }));
      buttons.set(tab.id, link);
      return link;
    }));
  return {
    nav,
    select(id) {
      for (const [tab, link] of buttons) {
        if (tab === id) link.setAttribute('aria-current', 'page');
        else link.removeAttribute('aria-current');
      }
    },
    badge(count) {
      badge.hidden = count === 0;
      badge.textContent = count > 99 ? '99+' : String(count);
      buttons.get('inbox').setAttribute('aria-label', count ? `Inbox, ${count} waiting` : 'Inbox');
    },
  };
}

function openCount(questions) {
  return (questions ?? []).filter((q) => !q.answer).length;
}

function renderBanner(banner, connection, app) {
  const { state, retryInMs, reason } = connection;
  banner.dataset.state = state;
  const message = {
    open: '',
    connecting: 'Connecting…',
    waiting: `Offline · retrying in ${Math.max(1, Math.round((retryInMs ?? 0) / 1000))} s`,
    disabled: 'Remote access is off on the host. Checking again in a minute.',
    revoked: 'This device is no longer paired.',
    stopped: '',
  }[state] ?? state;
  banner.replaceChildren();
  banner.classList.toggle('is-shown', Boolean(message));
  if (!message) return;
  banner.append(h('span', { class: 'conn-dot', 'aria-hidden': 'true' }), h('span', { text: message }));
  if (state === 'waiting') {
    banner.append(h('button', { class: 'link-button', type: 'button', onclick: () => app.transport.reconnectNow() }, 'Retry now'));
  }
  if (reason && state !== 'disabled') banner.title = reason;
}
