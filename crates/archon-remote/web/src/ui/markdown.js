// Markdown as DOM: the parsed tree is built with h(), so every character stays
// text and no HTML string is ever parsed. Links are filtered by safeHref().
import { h } from './dom.js';
import { parseMarkdown, parseInline } from './markdown-parse.js';

const MAX_RENDER_DEPTH = 24;

function inline(nodes, depth = 0) {
  return nodes.map((node) => {
    if (node.t === 'text') return node.v;
    if (node.t === 'br') return h('br');
    if (node.t === 'code') return h('code', { class: 'md-code' }, node.v);
    const kids = depth >= MAX_RENDER_DEPTH ? node.c.map((n) => (n.t === 'text' ? n.v : '')) : inline(node.c, depth + 1);
    if (node.t === 'link') {
      if (!node.href) return kids;
      const props = { class: 'md-link', href: node.href, title: node.title };
      if (node.external) Object.assign(props, { target: '_blank', rel: 'noopener noreferrer' });
      return h('a', props, kids);
    }
    return h({ em: 'em', strong: 'strong', del: 'del' }[node.t], {}, kids);
  });
}

const LANG = /^[A-Za-z0-9_+#.-]{1,32}/;

function block(b, tight = false) {
  switch (b.t) {
    case 'p': return tight ? inline(b.c) : h('p', {}, inline(b.c));
    case 'h': return h(`h${b.level}`, { class: 'md-heading' }, inline(b.c));
    case 'hr': return h('hr', { class: 'md-rule' });
    case 'code': {
      const lang = LANG.exec(b.info)?.[0];
      return h('pre', { class: 'md-pre' }, h('code', { class: lang ? `language-${lang}` : null }, b.text));
    }
    case 'quote': return h('blockquote', { class: 'md-quote' }, blocks(b.c));
    case 'ul':
    case 'ol': return h(b.t, { class: 'md-list', start: b.t === 'ol' && b.start !== 1 ? b.start : null },
      b.items.map((item) => h('li', { class: item.task === null ? null : 'md-task' },
        item.task === null ? null : h('input', { type: 'checkbox', class: 'md-check', disabled: true, checked: item.task, 'aria-label': item.task ? 'Done' : 'Not done' }),
        blocks(item.c, !b.loose))));
    case 'table': {
      const cell = (tag, nodes, k) => h(tag, { class: b.aligns[k] ? `align-${b.aligns[k]}` : null }, inline(nodes));
      return h('div', { class: 'md-scroll' }, h('table', { class: 'md-table' },
        h('thead', {}, h('tr', {}, b.headInline.map((c, k) => cell('th', c, k)))),
        b.rowsInline.length ? h('tbody', {}, b.rowsInline.map((row) => h('tr', {}, row.map((c, k) => cell('td', c, k))))) : null));
    }
    default: return null;
  }
}

function blocks(list, tight = false) {
  return list.map((b) => block(b, tight));
}

// The whole document as a <div class="md">.
export function renderMarkdown(source, { className } = {}) {
  return h('div', { class: ['md', className] }, blocks(parseMarkdown(source)));
}

// One line of prose (a title, a label that may carry `code`): inline marks only.
export function renderInline(source) {
  return h('span', { class: 'md-inline' }, inline(parseInline(String(source ?? '').replace(/\s+/g, ' ').trim())));
}

// Long Markdown folded by whole blocks: the first blocks that fit `limit`
// source characters (always at least one), then a toggle for the rest.
export function clampedMarkdown(source, { limit, more = 'Show all', less = 'Show less', className } = {}) {
  const all = parseMarkdown(source);
  let used = 0;
  let keep = 0;
  while (keep < all.length && (keep === 0 || used + all[keep].size <= limit)) { used += all[keep].size; keep += 1; }
  const folded = keep < all.length;
  const body = h('div', { class: ['md', className] }, blocks(folded ? all.slice(0, keep) : all));
  if (!folded) return body;
  const toggle = h('button', { class: 'link-button', type: 'button', 'aria-expanded': 'false' }, more);
  toggle.addEventListener('click', () => {
    const open = toggle.getAttribute('aria-expanded') === 'true';
    body.replaceChildren(...blocks(open ? all.slice(0, keep) : all).flat(Infinity).filter(Boolean).map((n) => (n instanceof Node ? n : document.createTextNode(String(n)))));
    toggle.setAttribute('aria-expanded', String(!open));
    toggle.textContent = open ? more : less;
  });
  return h('div', { class: 'md-clamp' }, body, toggle);
}
