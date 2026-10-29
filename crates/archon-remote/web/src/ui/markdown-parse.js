// Markdown to a plain tree: CommonMark blocks and inlines plus the GFM tables,
// strikethrough, task lists and bare autolinks. No DOM, no HTML: raw HTML is
// never recognised, so `<script>` is just the characters it is written with.
// Nesting is capped and every scan is bounded, so hostile input stays linear-ish.
//
// Block:  { t: 'p'|'h'|'quote'|'ul'|'ol'|'code'|'hr'|'table', ... }
// Inline: { t: 'text'|'code'|'br'|'em'|'strong'|'del'|'link', ... }

export const MAX_SOURCE_CHARS = 200_000;
export const MAX_BLOCK_DEPTH = 16;
const MAX_INLINE_DEPTH = 24;
const MAX_TABLE_COLUMNS = 64;
const MAX_TABLE_CELLS = 20_000;
const MAX_URL_CHARS = 1000;
const MAX_TITLE_CHARS = 1024;

const ENTITIES = {
  amp: '&', lt: '<', gt: '>', quot: '"', apos: "'", nbsp: ' ', copy: '©', reg: '®', trade: '™',
  hellip: '…', mdash: '—', ndash: '–', lsquo: '‘', rsquo: '’', ldquo: '“', rdquo: '”', middot: '·',
  bull: '•', times: '×', euro: '€', larr: '←', rarr: '→', uarr: '↑', darr: '↓', deg: '°', plusmn: '±',
  colon: ':', lpar: '(', rpar: ')', sol: '/', num: '#', excl: '!', quest: '?', ast: '*', lowbar: '_',
  grave: '`', Tab: '\t', NewLine: '\n',
};
const ENTITY_AT = /&(#[0-9]{1,7}|#[xX][0-9a-fA-F]{1,6}|[A-Za-z][A-Za-z0-9]{1,31});/y;
const ESCAPABLE = /[!-/:-@[-`{-~]/;

function entityText(body) {
  if (body[0] === '#') {
    const code = body[1] === 'x' || body[1] === 'X' ? parseInt(body.slice(2), 16) : parseInt(body.slice(1), 10);
    return code > 0 && code <= 0x10ffff && !(code >= 0xd800 && code <= 0xdfff) ? String.fromCodePoint(code) : '�';
  }
  return Object.hasOwn(ENTITIES, body) ? ENTITIES[body] : null;
}

// Backslash escapes and entities, as in link destinations and titles.
function unescapeText(s) {
  return s.replace(/\\([!-/:-@[-`{-~])|&(#[0-9]{1,7}|#[xX][0-9a-fA-F]{1,6}|[A-Za-z][A-Za-z0-9]{1,31});/g,
    (all, esc, ent) => (esc ?? entityText(ent) ?? all));
}

// ---- Links ----------------------------------------------------------------

// The only hrefs the app will emit: http(s), mailto, and its own #/ routes.
// Anything else, however it is disguised, is refused (null).
export function safeHref(raw) {
  const s = trimEndWhile(String(raw ?? ''), (c) => c <= 32).replace(/^[\x00-\x20]+/, '');
  if (!s || /[\x00-\x20\x7f-\x9f\u2028\u2029\ufeff]/.test(s)) return null;
  if (/^#\/[A-Za-z0-9\-._~!$&'()*+,;=:@/%?]*$/.test(s)) return { href: s, external: false };
  const scheme = /^([A-Za-z][A-Za-z0-9+.-]*):/.exec(s);
  if (!scheme) return null;
  const name = scheme[1].toLowerCase();
  if (name !== 'http' && name !== 'https' && name !== 'mailto') return null;
  try {
    const url = new URL(s);
    if (name !== 'mailto' && !url.hostname) return null;
  } catch {
    return null;
  }
  return { href: s, external: true };
}

// ---- Blocks ---------------------------------------------------------------

const isBlank = (l) => !/\S/.test(l);
const indentOf = (l) => /^ */.exec(l)[0].length;
// End-anchored regexes retry from every start on a long inner run, so trim
// from the end by hand.
function trimEndWhile(s, test) {
  let n = s.length;
  while (n > 0 && test(s.charCodeAt(n - 1))) n -= 1;
  return s.slice(0, n);
}
const isSpTab = (c) => c === 32 || c === 9;

// An ATX heading's text without its optional closing run of #.
function atxText(raw) {
  let s = trimEndWhile(raw, isSpTab);
  const hashes = trimEndWhile(s, (c) => c === 35);
  if (hashes.length === 0) return '';
  if (hashes.length < s.length && isSpTab(hashes.charCodeAt(hashes.length - 1))) s = trimEndWhile(hashes, isSpTab);
  return s.trim();
}

const HR = /^ {0,3}([-*_])(?:[ \t]*\1){2,}[ \t]*$/;
const ATX = /^ {0,3}(#{1,6})(?:[ \t]+([^]*))?$/;
const FENCE = /^( {0,3})(`{3,}|~{3,})[ \t]*([^]*)$/;
const LIST = /^( {0,3})([-+*]|\d{1,9}[.)])( +|$)/;
const SETEXT = /^ {0,3}(=+|-+)[ \t]*$/;
const QUOTE = /^ {0,3}> ?/;
const DELIM_ROW = /^ {0,3}\|?[ \t]*:?-+:?[ \t]*(?:\|[ \t]*:?-+:?[ \t]*)*(?:\|[ \t]*)?$/;
const REF_DEF = /^ {0,3}\[((?:[^\]\\]|\\.){1,999})\]:[ \t]*(<[^<>]*>|\S+)(?:[ \t]+("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|\((?:[^)\\]|\\.)*\)))?[ \t]*$/;

function normalizeLabel(label) {
  return label.trim().replace(/\s+/g, ' ').toLowerCase();
}

function expandTabs(line) {
  if (!line.includes('\t')) return line;
  const lead = /^[ \t]*/.exec(line)[0];
  let col = 0;
  for (const ch of lead) col = ch === '\t' ? col + 4 - (col % 4) : col + 1;
  return ' '.repeat(col) + line.slice(lead.length);
}

function isFenceOpen(line) {
  const f = FENCE.exec(line);
  return f && !(f[2][0] === '`' && f[3].includes('`')) ? f : null;
}

// Can this line interrupt a paragraph (or end a lazy continuation)?
function startsBlock(line, depth) {
  if (indentOf(line) >= 4) return false;
  if (isFenceOpen(line) || ATX.test(line) || HR.test(line)) return true;
  if (depth >= MAX_BLOCK_DEPTH) return false;
  if (QUOTE.test(line)) return true;
  const m = LIST.exec(line);
  if (!m || m[3] === '' || !line.slice(m[0].length).trim()) return false;
  return !/^\d/.test(m[2]) || parseInt(m[2], 10) === 1;
}

function splitRow(line) {
  let s = line.trim();
  if (s.startsWith('|')) s = s.slice(1);
  const cells = [];
  let cur = '';
  for (let i = 0; i < s.length; i += 1) {
    const ch = s[i];
    if (ch === '\\' && i + 1 < s.length) {
      cur += s[i + 1] === '|' ? '|' : ch + s[i + 1];
      i += 1;
    } else if (ch === '|') {
      cells.push(cur.trim());
      cur = '';
    } else cur += ch;
  }
  if (cur.trim() !== '' || cells.length === 0) cells.push(cur.trim());
  return cells;
}

function tableAt(lines, i) {
  if (i + 1 >= lines.length) return null;
  const head = lines[i];
  const delim = lines[i + 1];
  if (!DELIM_ROW.test(delim) || (!head.includes('|') && !delim.includes('|')) || indentOf(head) >= 4) return null;
  const heads = splitRow(head);
  const aligns = splitRow(delim);
  if (heads.length !== aligns.length || heads.length > MAX_TABLE_COLUMNS) return null;
  return { heads, aligns: aligns.map((a) => (a.startsWith(':') ? (a.endsWith(':') ? 'center' : 'left') : (a.endsWith(':') ? 'right' : null))) };
}

function parseBlocks(lines, depth, ctx) {
  const blocks = [];
  const n = lines.length;
  let i = 0;
  let gap = false;
  const push = (block, from) => {
    block.gap = gap && blocks.length > 0;
    if (depth === 0) block.size = lines.slice(from, i).reduce((sum, l) => sum + l.length + 1, 0);
    gap = false;
    blocks.push(block);
  };
  while (i < n) {
    const line = lines[i];
    if (isBlank(line)) { gap = true; i += 1; continue; }
    const from = i;
    const ind = indentOf(line);

    if (ind >= 4) {
      const code = [];
      let j = i;
      while (j < n && (isBlank(lines[j]) || indentOf(lines[j]) >= 4)) { code.push(lines[j].slice(4)); j += 1; }
      while (code.length && isBlank(code.at(-1))) code.pop();
      i += code.length;
      push({ t: 'code', info: '', text: code.join('\n') }, from);
      continue;
    }

    const fence = isFenceOpen(line);
    if (fence) {
      const [, lead, marks, info] = fence;
      const body = [];
      i += 1;
      while (i < n) {
        const close = /^ {0,3}([`~]+)[ \t]*$/.exec(lines[i]);
        if (close && close[1][0] === marks[0] && close[1].length >= marks.length) { i += 1; break; }
        body.push(lines[i].replace(new RegExp(`^ {0,${lead.length}}`), ''));
        i += 1;
      }
      push({ t: 'code', info: unescapeText(info.trim()), text: body.join('\n') }, from);
      continue;
    }

    const atx = ATX.exec(line);
    if (atx) {
      const text = atxText(atx[2] ?? '');
      i += 1;
      push({ t: 'h', level: atx[1].length, text }, from);
      continue;
    }

    if (HR.test(line)) { i += 1; push({ t: 'hr' }, from); continue; }

    if (depth < MAX_BLOCK_DEPTH && QUOTE.test(line)) {
      const inner = [];
      while (i < n) {
        const q = QUOTE.exec(lines[i]);
        if (q) inner.push(lines[i].slice(q[0].length));
        else if (!isBlank(lines[i]) && inner.length && !isBlank(inner.at(-1)) && !startsBlock(lines[i], depth)) inner.push(lines[i]);
        else break;
        i += 1;
      }
      push({ t: 'quote', c: parseBlocks(inner, depth + 1, ctx) }, from);
      continue;
    }

    const lm = depth < MAX_BLOCK_DEPTH ? LIST.exec(line) : null;
    if (lm) {
      const list = parseList(lines, i, depth, ctx);
      i = list.next;
      push(list.block, from);
      continue;
    }

    const table = tableAt(lines, i);
    if (table) {
      i += 2;
      const rows = [];
      while (i < n && !isBlank(lines[i]) && !startsBlock(lines[i], depth) && (rows.length + 1) * table.heads.length <= MAX_TABLE_CELLS) {
        const cells = splitRow(lines[i]);
        rows.push(table.heads.map((_, k) => cells[k] ?? ''));
        i += 1;
      }
      push({ t: 'table', aligns: table.aligns, head: table.heads, rows }, from);
      continue;
    }

    const para = [];
    let level = 0;
    while (i < n && !isBlank(lines[i])) {
      const l = lines[i];
      if (para.length) {
        const setext = SETEXT.exec(l);
        if (setext && indentOf(l) < 4) { level = setext[1][0] === '=' ? 1 : 2; i += 1; break; }
        if (startsBlock(l, depth) || (l.includes('|') && tableAt(lines, i))) break;
      }
      para.push(l.replace(/^ +/, ''));
      i += 1;
    }
    while (!level && para.length) {
      const def = REF_DEF.exec(para[0]);
      if (!def) break;
      const key = normalizeLabel(def[1]);
      if (key && !ctx.refs.has(key)) {
        const dest = def[2].startsWith('<') ? def[2].slice(1, -1) : def[2];
        ctx.refs.set(key, { href: unescapeText(dest), title: def[3] ? unescapeText(def[3].slice(1, -1)) : null });
      }
      para.shift();
    }
    if (!para.length) continue;
    const text = trimEndWhile(para.join('\n'), isSpTab);
    push(level ? { t: 'h', level, text } : { t: 'p', text }, from);
  }
  return blocks;
}

function parseList(lines, start, depth, ctx) {
  const n = lines.length;
  const first = LIST.exec(lines[start]);
  const ordered = /^\d/.test(first[2]);
  const kind = ordered ? first[2].slice(-1) : first[2];
  const items = [];
  let loose = false;
  let i = start;
  const sameKind = (m) => /^\d/.test(m[2]) === ordered && (ordered ? m[2].slice(-1) : m[2]) === kind;
  while (i < n) {
    const m = LIST.exec(lines[i]);
    if (!m || HR.test(lines[i]) || !sameKind(m)) break;
    const rest = lines[i].slice(m[0].length);
    const markerEnd = m[1].length + m[2].length;
    let contentIndent;
    let firstLine;
    if (!rest.trim()) { contentIndent = markerEnd + 1; firstLine = ''; }
    else if (m[3].length >= 5) { contentIndent = markerEnd + 1; firstLine = ' '.repeat(m[3].length - 1) + rest; }
    else { contentIndent = markerEnd + m[3].length; firstLine = rest; }
    const body = [firstLine];
    i += 1;
    while (i < n) {
      const l = lines[i];
      if (isBlank(l)) {
        let j = i;
        while (j < n && isBlank(lines[j])) j += 1;
        // An item may start with one blank line, no more.
        if (j >= n || indentOf(lines[j]) < contentIndent || (body.length === 1 && body[0] === '' && j - i > 0)) break;
        for (; i < j; i += 1) body.push('');
      } else if (indentOf(l) >= contentIndent) {
        body.push(l.slice(contentIndent));
        i += 1;
      } else if (!isBlank(body.at(-1)) && !LIST.test(l) && !startsBlock(l, depth)) {
        body.push(l);
        i += 1;
      } else break;
    }
    const children = parseBlocks(body, depth + 1, ctx);
    if (children.some((b, k) => k > 0 && b.gap)) loose = true;
    const item = { c: children, task: null };
    const lead = children[0];
    if (lead?.t === 'p') {
      const task = /^\[([ xX])\](?:[ \t]+|$)/.exec(lead.text);
      if (task && lead.text.length > task[0].length) {
        item.task = task[1] !== ' ';
        lead.text = lead.text.slice(task[0].length);
      }
    }
    items.push(item);
    // The next sibling, with any blank lines before it.
    let j = i;
    while (j < n && isBlank(lines[j])) j += 1;
    const sib = j < n ? LIST.exec(lines[j]) : null;
    if (!sib || HR.test(lines[j]) || indentOf(lines[j]) >= contentIndent || !sameKind(sib)) break;
    if (j > i) loose = true;
    i = j;
  }
  const block = { t: ordered ? 'ol' : 'ul', start: ordered ? parseInt(first[2], 10) : 1, loose, items };
  return { block, next: i };
}

// ---- Inlines --------------------------------------------------------------

const PUNCT = /[\p{P}\p{S}]/u;
const SPACE = /\s/;
const SPECIAL = /[\\`*_~[\]!<&\nhwHW]/g;
const URL_AT = /(?:https?:\/\/|www\.)[^\s<]{1,2048}/iy;
const AUTOLINK = /<([A-Za-z][A-Za-z0-9+.-]{1,31}:[^\s<>]{0,2048})>/y;
const EMAIL = /<([A-Za-z0-9.!#$%&'*+/=?^_`{|}~-]{1,64}@[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?(?:\.[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?){0,8})>/y;

function trimUrlTail(url) {
  let s = url;
  let close = 0;
  let open = 0;
  for (const c of s) { if (c === ')') close += 1; else if (c === '(') open += 1; }
  for (;;) {
    const last = s.at(-1);
    if (last && '?!.,:*_~\'"'.includes(last)) s = s.slice(0, -1);
    else if (last === ')' && close > open) { s = s.slice(0, -1); close -= 1; }
    else if (last === ';' && /&[A-Za-z0-9]+;$/.test(s)) s = s.replace(/&[A-Za-z0-9]+;$/, '');
    else return s;
  }
}

// Inline source to nodes. `refs` resolves [text][label] references.
export function parseInline(src, refs = new Map()) {
  let first = null;
  let last = null;
  const append = (node) => {
    node.prev = last;
    node.next = null;
    if (last) last.next = node; else first = node;
    last = node;
    return node;
  };
  const unlink = (node) => {
    if (node.prev) node.prev.next = node.next; else first = node.next;
    if (node.next) node.next.prev = node.prev; else last = node.prev;
  };
  let delims = null; // delimiter stack, a doubly linked list; `delims` is its top
  const brackets = [];
  // Link openers below this stack index are off: a link cannot hold a link.
  let linkFloor = 0;
  let buf = '';
  let pos = 0;
  const autolink = (href, text) => ({ t: 'link', href, title: null, image: false, depth: 1, inner: { t: 'text', v: text, prev: null, next: null } });
  const flush = () => { if (buf) { append({ t: 'text', v: buf }); buf = ''; } };

  const tickRuns = new Map();
  let ticksReady = false;
  const closingTicks = (len, from) => {
    if (!ticksReady) {
      ticksReady = true;
      for (const m of src.matchAll(/`+/g)) {
        if (!tickRuns.has(m[0].length)) tickRuns.set(m[0].length, { at: [], k: 0 });
        tickRuns.get(m[0].length).at.push(m.index);
      }
    }
    const runs = tickRuns.get(len);
    if (!runs) return -1;
    while (runs.k < runs.at.length && runs.at[runs.k] < from) runs.k += 1;
    return runs.k < runs.at.length ? runs.at[runs.k] : -1;
  };

  const parseTail = (at) => {
    // [text](dest "title"), from the "(" at `at`; returns { href, title, end } or null.
    let p = at + 1;
    const ws = () => { while (p < src.length && /[ \t\n]/.test(src[p])) p += 1; };
    ws();
    let dest;
    if (src[p] === '<') {
      const from = p + 1;
      p = from;
      while (p < src.length && p - from <= MAX_URL_CHARS && src[p] !== '>' && src[p] !== '<' && src[p] !== '\n') p += src[p] === '\\' ? 2 : 1;
      if (src[p] !== '>') return null;
      dest = src.slice(from, p);
      p += 1;
    } else {
      const from = p;
      let depth = 0;
      while (p < src.length && p - from <= MAX_URL_CHARS && !/[\s\u0000-\u001f\u007f]/.test(src[p])) {
        if (src[p] === '\\' && p + 1 < src.length) { p += 2; continue; }
        if (src[p] === '(') depth += 1;
        else if (src[p] === ')') { if (depth === 0) break; depth -= 1; }
        p += 1;
      }
      if (depth !== 0 || p - from > MAX_URL_CHARS) return null;
      dest = src.slice(from, p);
    }
    const afterDest = p;
    ws();
    let title = null;
    const open = src[p];
    if (p > afterDest && (open === '"' || open === "'" || open === '(')) {
      const close = open === '(' ? ')' : open;
      const from = p + 1;
      p = from;
      while (p < src.length && p - from <= MAX_TITLE_CHARS && src[p] !== close) p += src[p] === '\\' ? 2 : 1;
      if (src[p] !== close) return null;
      title = unescapeText(src.slice(from, p));
      p += 1;
      ws();
    }
    if (src[p] !== ')') return null;
    return { href: unescapeText(dest), title, end: p + 1 };
  };

  // Nesting depth of a chain. The scans share one budget, so flat input
  // cannot make repeated wrapping quadratic; once spent, nothing new nests.
  let budget = 8 * src.length + 1000;
  const depthOf = (head, stop = null) => {
    let d = 0;
    for (let n = head; n && n !== stop; n = n.next) {
      budget -= 1;
      if (budget < 0) return Infinity;
      d = Math.max(d, n.depth ?? 0);
    }
    return d;
  };
  // Wrap the nodes strictly between `from` and `to` in a new node; null when empty or too deep.
  const wrapBetween = (from, to, type, extra) => {
    const head = from.next;
    if (head === to) return null;
    const tail = to.prev;
    const depth = depthOf(head, to) + 1;
    if (depth > MAX_INLINE_DEPTH) return null;
    const node = { t: type, depth, ...extra, inner: head, prev: from, next: to };
    head.prev = null;
    tail.next = null;
    from.next = node;
    to.prev = node;
    return node;
  };

  const removeDelim = (d) => {
    if (d.prev) d.prev.next = d.next;
    if (d.next) d.next.prev = d.prev; else delims = d.prev;
  };

  const processEmphasis = (bottom) => {
    const bottoms = new Map();
    let closer = bottom ? bottom.next : (() => { let d = delims; while (d?.prev) d = d.prev; return d; })();
    while (closer) {
      if (!closer.canClose) { closer = closer.next; continue; }
      const key = `${closer.ch}${closer.orig % 3}${closer.canOpen ? 1 : 0}`;
      const limit = bottoms.has(key) ? bottoms.get(key) : bottom;
      let opener = closer.prev;
      let found = false;
      while (opener && opener !== limit) {
        const odd = (closer.canOpen || opener.canClose) && closer.orig % 3 !== 0 && (opener.orig + closer.orig) % 3 === 0;
        if (opener.ch === closer.ch && opener.canOpen && !odd && (closer.ch !== '~' || opener.count === closer.count)) { found = true; break; }
        opener = opener.prev;
      }
      const old = closer;
      if (found) {
        const use = closer.ch === '~' ? 2 : (closer.count >= 2 && opener.count >= 2 ? 2 : 1);
        opener.count -= use;
        closer.count -= use;
        opener.node.v = opener.node.v.slice(0, opener.node.v.length - use);
        closer.node.v = closer.node.v.slice(use);
        wrapBetween(opener.node, closer.node, closer.ch === '~' ? 'del' : (use === 2 ? 'strong' : 'em'), {});
        // Drop the delimiters between the pair.
        opener.next = closer;
        closer.prev = opener;
        if (opener.count === 0) { unlink(opener.node); removeDelim(opener); }
        if (closer.count === 0) {
          const next = closer.next;
          unlink(closer.node);
          removeDelim(closer);
          closer = next;
        }
      } else {
        closer = closer.next;
        bottoms.set(key, old.prev);
        if (!old.canOpen) removeDelim(old);
      }
    }
    // Everything above the bottom is spent.
    if (bottom) { bottom.next = null; delims = bottom; } else delims = null;
  };

  const flanking = (ch, start, end) => {
    const before = start > 0 ? src[start - 1] : '\n';
    const after = end < src.length ? src[end] : '\n';
    const bWS = SPACE.test(before);
    const aWS = SPACE.test(after);
    const bP = PUNCT.test(before);
    const aP = PUNCT.test(after);
    const left = !aWS && (!aP || bWS || bP);
    const right = !bWS && (!bP || aWS || aP);
    if (ch === '_') return { canOpen: left && (!right || bP), canClose: right && (!left || aP) };
    return { canOpen: left, canClose: right };
  };

  const closeBracket = () => {
    const opener = brackets.pop();
    pos += 1;
    const off = opener && !opener.image && brackets.length < linkFloor;
    if (linkFloor > brackets.length) linkFloor = brackets.length;
    if (!opener || off) { buf += ']'; return; }
    let target = null;
    let end = pos;
    if (src[end] === '(') {
      const tail = parseTail(end);
      if (tail) { target = tail; end = tail.end; }
    }
    if (!target && refs.size > 0 && pos - 1 - opener.at <= 999) {
      const labelSrc = src.slice(opener.at, pos - 1);
      let label = labelSrc;
      let stop = pos;
      if (src[stop] === '[') {
        const close = src.indexOf(']', stop + 1);
        if (close > 0 && close - stop <= 1000 && !src.slice(stop + 1, close).includes('[')) {
          label = close === stop + 1 ? labelSrc : src.slice(stop + 1, close);
          stop = close + 1;
        }
      }
      const ref = label.trim() ? refs.get(normalizeLabel(label)) : null;
      if (ref) { target = ref; end = stop; }
    }
    if (!target) { buf += ']'; return; }
    flush();
    processEmphasis(opener.delim);
    const label = opener.node;
    const inner = label.next;
    last = label.prev;
    if (last) last.next = null; else first = null;
    const link = { t: 'link', href: target.href, title: target.title, image: opener.image, inner, depth: 1 };
    if (inner) {
      inner.prev = null;
      link.depth = depthOf(inner) + 1;
      // Pathologically nested labels keep the link but drop the label.
      if (link.depth > MAX_INLINE_DEPTH) { link.inner = null; link.depth = 1; }
    }
    append(link);
    // Links do not nest: earlier link openers can no longer close.
    if (!opener.image) linkFloor = brackets.length;
    pos = end;
  };

  while (pos < src.length) {
    SPECIAL.lastIndex = pos;
    const hit = SPECIAL.exec(src);
    if (!hit) { buf += src.slice(pos); break; }
    buf += src.slice(pos, hit.index);
    pos = hit.index;
    const ch = src[pos];

    if (ch === '\\') {
      if (src[pos + 1] === '\n') { flush(); append({ t: 'br' }); pos += 2; }
      else if (pos + 1 < src.length && ESCAPABLE.test(src[pos + 1])) { buf += src[pos + 1]; pos += 2; }
      else { buf += '\\'; pos += 1; }
    } else if (ch === '\n') {
      // Count the spaces in the source, not the buffer: reading a long
      // buffer back would flatten it on every line.
      let spaces = 0;
      while (src.charCodeAt(pos - 1 - spaces) === 32 && spaces < buf.length) spaces += 1;
      const hard = spaces >= 2;
      if (spaces > 0) buf = buf.slice(0, buf.length - spaces);
      if (hard) { flush(); append({ t: 'br' }); }
      else buf += '\n';
      pos += 1;
    } else if (ch === '`') {
      let end = pos;
      while (src[end] === '`') end += 1;
      const len = end - pos;
      const close = closingTicks(len, end);
      if (close < 0) { buf += src.slice(pos, end); pos = end; continue; }
      let code = src.slice(end, close).replace(/\n/g, ' ');
      if (code.length > 2 && code.startsWith(' ') && code.endsWith(' ') && /[^ ]/.test(code)) code = code.slice(1, -1);
      flush();
      append({ t: 'code', v: code });
      pos = close + len;
    } else if (ch === '*' || ch === '_' || ch === '~') {
      let end = pos;
      while (src[end] === ch) end += 1;
      const len = end - pos;
      if (ch === '~' && len !== 2) { buf += src.slice(pos, end); pos = end; continue; }
      const { canOpen, canClose } = flanking(ch, pos, end);
      if (!canOpen && !canClose) { buf += src.slice(pos, end); pos = end; continue; }
      flush();
      const node = append({ t: 'text', v: src.slice(pos, end) });
      const d = { ch, count: len, orig: len, node, canOpen, canClose, prev: delims, next: null };
      if (delims) delims.next = d;
      delims = d;
      pos = end;
    } else if (ch === '[' || (ch === '!' && src[pos + 1] === '[')) {
      const image = ch === '!';
      flush();
      const node = append({ t: 'text', v: image ? '![' : '[' });
      brackets.push({ node, at: pos + (image ? 2 : 1), image, delim: delims });
      pos += image ? 2 : 1;
    } else if (ch === '!') {
      buf += '!';
      pos += 1;
    } else if (ch === ']') {
      closeBracket();
    } else if (ch === '<') {
      AUTOLINK.lastIndex = pos;
      const am = AUTOLINK.exec(src);
      EMAIL.lastIndex = pos;
      const em = am ?? EMAIL.exec(src);
      if (em && safeHref(am ? em[1] : `mailto:${em[1]}`)) {
        flush();
        append(autolink(am ? em[1] : `mailto:${em[1]}`, em[1]));
        pos += em[0].length;
      } else { buf += '<'; pos += 1; }
    } else if (ch === '&') {
      ENTITY_AT.lastIndex = pos;
      const em = ENTITY_AT.exec(src);
      const text = em ? entityText(em[1]) : null;
      if (text === null) { buf += '&'; pos += 1; } else { buf += text; pos += em[0].length; }
    } else {
      // h / w: maybe a bare URL, at a word start and outside link text.
      const prev = src[pos - 1];
      let url = '';
      if (brackets.length === 0 && (prev === undefined || SPACE.test(prev) || '*_~('.includes(prev))) {
        URL_AT.lastIndex = pos;
        const um = URL_AT.exec(src);
        const text = um ? trimUrlTail(um[0]) : '';
        const www = /^www\./i.test(text);
        const host = /^[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*/.exec(www ? text.slice(4) : text.replace(/^https?:\/\//i, ''))?.[0] ?? '';
        if (host && (!www || host.includes('.')) && safeHref(www ? `http://${text}` : text)) url = text;
      }
      if (url) {
        flush();
        append(autolink(/^www\./i.test(url) ? `http://${url}` : url, url));
        pos += url.length;
      } else { buf += ch; pos += 1; }
    }
  }
  flush();
  processEmphasis(null);
  return materialize(first, 0);
}

// Linked chain to arrays, merging neighbouring text; images and unsafe links flatten.
function materialize(head, depth) {
  const out = [];
  for (let n = head; n; n = n.next) {
    if (n.t === 'text') {
      if (!n.v) continue;
      if (out.at(-1)?.t === 'text') out.at(-1).v += n.v; else out.push({ t: 'text', v: n.v });
    } else if (n.t === 'code') out.push({ t: 'code', v: n.v });
    else if (n.t === 'br') out.push({ t: 'br' });
    else {
      const c = materialize(n.inner, depth + 1);
      if (n.t === 'link') {
        const safe = safeHref(n.href);
        out.push({ t: 'link', href: safe?.href ?? null, external: safe?.external ?? false, title: n.title ?? null, image: Boolean(n.image),
          c: n.image ? [{ t: 'text', v: plainText(c) || 'image' }] : c });
      } else out.push({ t: n.t, c });
    }
  }
  return out;
}

export function plainText(nodes) {
  return nodes.map((n) => (n.t === 'text' || n.t === 'code' ? n.v : n.t === 'br' ? ' ' : plainText(n.c ?? []))).join('');
}

// ---- Entry ----------------------------------------------------------------

function inlineAll(blocks, refs) {
  for (const b of blocks) {
    if (b.t === 'p' || b.t === 'h') b.c = parseInline(b.text, refs);
    else if (b.t === 'quote') inlineAll(b.c, refs);
    else if (b.t === 'ul' || b.t === 'ol') for (const item of b.items) inlineAll(item.c, refs);
    else if (b.t === 'table') {
      b.headInline = b.head.map((cell) => parseInline(cell, refs));
      b.rowsInline = b.rows.map((row) => row.map((cell) => parseInline(cell, refs)));
    }
  }
  return blocks;
}

export function parseMarkdown(source) {
  let text = String(source ?? '');
  const truncated = text.length > MAX_SOURCE_CHARS;
  if (truncated) text = `${text.slice(0, MAX_SOURCE_CHARS)}\n\n…`;
  const lines = text.replace(/\r\n?/g, '\n').replace(/\u0000/g, '�').split('\n').map(expandTabs);
  const ctx = { refs: new Map() };
  return inlineAll(parseBlocks(lines, 0, ctx), ctx.refs);
}
