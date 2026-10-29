// Markdown to a plain tree: CommonMark blocks and inlines plus the GFM tables,
// strikethrough, task lists and bare autolinks. No DOM, no HTML: raw HTML is
// never recognised, so `<script>` is just the characters it is written with.
// Nesting is capped and every scan is bounded, so hostile input stays linear-ish.
//
// Block:  { t: 'p'|'h'|'quote'|'ul'|'ol'|'code'|'hr'|'table', ... }
// Inline: { t: 'text'|'code'|'br'|'em'|'strong'|'del'|'link', ... }

import { ENTITIES } from './entities.js';

export const MAX_SOURCE_CHARS = 200_000;
export const MAX_BLOCK_DEPTH = 16;
const MAX_INLINE_DEPTH = 24;
const MAX_TABLE_COLUMNS = 64;
const MAX_TABLE_CELLS = 20_000;
const MAX_URL_CHARS = 1000;
const MAX_TITLE_CHARS = 1024;

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
const CLOSE_FENCE = /^ {0,3}(`+|~+)[ \t]*$/;
const LIST = /^( {0,3})([-+*]|\d{1,9}[.)])([ \t]+|$)/;
const SETEXT = /^ {0,3}(=+|-+)[ \t]*$/;
const QUOTE = /^ {0,3}>[ \t]?/;
const DELIM_ROW = /^ {0,3}\|?[ \t]*:?-+:?[ \t]*(?:\|[ \t]*:?-+:?[ \t]*)*(?:\|[ \t]*)?$/;
const REF_DEF = /^ {0,3}\[((?:[^\]\\]|\\.){1,999})\]:[ \t]*(<[^<>]*>|\S+)(?:[ \t]+("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|\((?:[^)\\]|\\.)*\)))?[ \t]*$/;

function normalizeLabel(label) {
  return label.trim().replace(/\s+/g, ' ').toLowerCase();
}

// The columns a run of spaces and tabs spans when it starts at column `start`.
function columnsOf(run, start) {
  let col = start;
  for (const ch of run) col = ch === '\t' ? col + 4 - (col % 4) : col + 1;
  return col - start;
}

// Leading tabs as spaces, for a line whose first character sits at column `start`.
function expandTabs(line, start = 0) {
  if (!line.includes('\t')) return line;
  const lead = /^[ \t]*/.exec(line)[0];
  return ' '.repeat(columnsOf(lead, start)) + line.slice(lead.length);
}

// A quoted line's content after its `>` marker; a tab after `>` gives up one column to it.
function quoteRest(line, marker) {
  const rest = line.slice(marker.length);
  if (!marker.endsWith('\t')) return expandTabs(rest, marker.length);
  const tabAt = marker.length - 1;
  const width = 4 - (tabAt % 4);
  return ' '.repeat(width - 1) + expandTabs(rest, tabAt + width);
}

function isFenceOpen(line) {
  const f = FENCE.exec(line);
  return f && !(f[2][0] === '`' && f[3].includes('`')) ? f : null;
}

// A line without the quote and list markers that open it.
function containerContent(line) {
  let s = line;
  for (let k = 0; k < MAX_BLOCK_DEPTH; k += 1) {
    const m = QUOTE.exec(s) ?? LIST.exec(s);
    if (!m) break;
    s = s.slice(m[0].length);
  }
  return s;
}

// Follows a container's lines to know whether its content ends in an open
// paragraph, the only block an unmarked (lazy) line may continue.
function trackParagraph(open, line) {
  if (open.fence) {
    const close = CLOSE_FENCE.exec(line);
    if (close && close[1][0] === open.fence[0] && close[1].length >= open.fence.length) open.fence = null;
    open.paragraph = false;
    return;
  }
  const fence = isFenceOpen(line);
  if (fence) { open.fence = fence[2]; open.paragraph = false; return; }
  const content = containerContent(line);
  if (isBlank(content) || ATX.test(content) || HR.test(line) || (open.paragraph && SETEXT.test(line))) { open.paragraph = false; return; }
  if (indentOf(line) >= 4 && !open.paragraph) return;
  open.paragraph = true;
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
  if (depth > MAX_BLOCK_DEPTH) throw new RangeError(`block nesting ${depth} is past the cap`);
  const blocks = [];
  let i = 0;
  let gap = false;
  while (i < lines.length) {
    if (isBlank(lines[i])) { gap = true; i += 1; continue; }
    const { block, next } = readBlock(lines, i, depth, ctx);
    if (next <= i) throw new Error(`the block reader stalled at line ${i}`);
    if (block) {
      block.gap = gap && blocks.length > 0;
      if (depth === 0) block.size = lines.slice(i, next).reduce((sum, l) => sum + l.length + 1, 0);
      gap = false;
      blocks.push(block);
    }
    i = next;
  }
  return blocks;
}

// The block starting at the non-blank line `i`, and the line after it.
function readBlock(lines, i, depth, ctx) {
  const line = lines[i];
  if (indentOf(line) >= 4) return indentedCode(lines, i);
  const fence = isFenceOpen(line);
  if (fence) return fencedCode(lines, i, fence);
  const atx = ATX.exec(line);
  if (atx) return { block: { t: 'h', level: atx[1].length, text: atxText(atx[2] ?? '') }, next: i + 1 };
  if (HR.test(line)) return { block: { t: 'hr' }, next: i + 1 };
  if (depth < MAX_BLOCK_DEPTH && QUOTE.test(line)) return blockQuote(lines, i, depth, ctx);
  if (depth < MAX_BLOCK_DEPTH && LIST.test(line)) return parseList(lines, i, depth, ctx);
  const table = tableAt(lines, i);
  if (table) return tableRows(lines, i, table, depth);
  return paragraph(lines, i, depth, ctx);
}

function indentedCode(lines, start) {
  const code = [];
  let j = start;
  while (j < lines.length && (isBlank(lines[j]) || indentOf(lines[j]) >= 4)) { code.push(lines[j].slice(4)); j += 1; }
  while (code.length && isBlank(code.at(-1))) code.pop();
  return { block: { t: 'code', info: '', text: code.join('\n') }, next: start + code.length };
}

function fencedCode(lines, start, [, lead, marks, info]) {
  const body = [];
  const indent = new RegExp(`^ {0,${lead.length}}`);
  let i = start + 1;
  while (i < lines.length) {
    const close = CLOSE_FENCE.exec(lines[i]);
    if (close && close[1][0] === marks[0] && close[1].length >= marks.length) { i += 1; break; }
    body.push(lines[i].replace(indent, ''));
    i += 1;
  }
  return { block: { t: 'code', info: unescapeText(info.trim()), text: body.join('\n') }, next: i };
}

function blockQuote(lines, start, depth, ctx) {
  const inner = [];
  const open = { paragraph: false, fence: null };
  let i = start;
  while (i < lines.length) {
    const q = QUOTE.exec(lines[i]);
    if (q) inner.push(quoteRest(lines[i], q[0]));
    else if (open.paragraph && !isBlank(lines[i]) && !startsBlock(lines[i], depth)) inner.push(lines[i]);
    else break;
    trackParagraph(open, inner.at(-1));
    i += 1;
  }
  return { block: { t: 'quote', c: parseBlocks(inner, depth + 1, ctx) }, next: i };
}

function tableRows(lines, start, table, depth) {
  const rows = [];
  let i = start + 2;
  while (i < lines.length && !isBlank(lines[i]) && !startsBlock(lines[i], depth) && (rows.length + 1) * table.heads.length <= MAX_TABLE_CELLS) {
    const cells = splitRow(lines[i]);
    rows.push(table.heads.map((_, k) => cells[k] ?? ''));
    i += 1;
  }
  return { block: { t: 'table', aligns: table.aligns, head: table.heads, rows }, next: i };
}

// A paragraph or setext heading; leading reference definitions go to ctx.refs,
// and a paragraph made only of them yields no block.
function paragraph(lines, start, depth, ctx) {
  const para = [];
  let level = 0;
  let i = start;
  while (i < lines.length && !isBlank(lines[i])) {
    const l = lines[i];
    if (para.length) {
      const setext = SETEXT.exec(l);
      if (setext && indentOf(l) < 4) { level = setext[1][0] === '=' ? 1 : 2; i += 1; break; }
      if (startsBlock(l, depth) || (l.includes('|') && tableAt(lines, i))) break;
    }
    para.push(l.replace(/^ +/, ''));
    i += 1;
  }
  if (!level) takeRefDefs(para, ctx.refs);
  if (!para.length) return { block: null, next: i };
  const text = trimEndWhile(para.join('\n'), isSpTab);
  return { block: level ? { t: 'h', level, text } : { t: 'p', text }, next: i };
}

// Moves the reference definitions that open `para` into `refs`; the first one wins.
function takeRefDefs(para, refs) {
  let taken = 0;
  for (; taken < para.length; taken += 1) {
    const def = REF_DEF.exec(para[taken]);
    if (!def) break;
    const key = normalizeLabel(def[1]);
    if (key && !refs.has(key)) {
      const dest = def[2].startsWith('<') ? def[2].slice(1, -1) : def[2];
      // Like an inline destination, an overlong one is no link; each use would revalidate it.
      const href = dest.length <= MAX_URL_CHARS ? unescapeText(dest) : null;
      refs.set(key, { href, title: def[3] ? unescapeText(def[3].slice(1, -1)) : null });
    }
  }
  para.splice(0, taken);
}

function parseList(lines, start, depth, ctx) {
  const first = LIST.exec(lines[start]);
  if (!first) throw new Error(`line ${start} does not open a list`);
  const ordered = /^\d/.test(first[2]);
  const kind = ordered ? first[2].slice(-1) : first[2];
  const sameKind = (m) => /^\d/.test(m[2]) === ordered && (ordered ? m[2].slice(-1) : m[2]) === kind;
  const items = [];
  let loose = false;
  let i = start;
  while (i < lines.length) {
    const m = LIST.exec(lines[i]);
    if (!m || HR.test(lines[i]) || !sameKind(m)) break;
    const { body, next, contentIndent } = listItemBody(lines, i, m, depth);
    const children = parseBlocks(body, depth + 1, ctx);
    if (children.some((b, k) => k > 0 && b.gap)) loose = true;
    items.push(taskItem(children));
    i = next;
    // The next sibling, with any blank lines before it.
    let j = i;
    while (j < lines.length && isBlank(lines[j])) j += 1;
    const sib = j < lines.length ? LIST.exec(lines[j]) : null;
    if (!sib || HR.test(lines[j]) || indentOf(lines[j]) >= contentIndent || !sameKind(sib)) break;
    if (j > i) loose = true;
    i = j;
  }
  if (items.length === 0) throw new Error(`the list at line ${start} has no items`);
  const block = { t: ordered ? 'ol' : 'ul', start: ordered ? parseInt(first[2], 10) : 1, loose, items };
  return { block, next: i };
}

// One item's lines, its marker `m` at line `start` removed and its indent stripped.
function listItemBody(lines, start, m, depth) {
  const n = lines.length;
  const rest = lines[start].slice(m[0].length);
  const markerEnd = m[1].length + m[2].length;
  const spacing = columnsOf(m[3], markerEnd);
  let contentIndent;
  let firstLine;
  if (!rest.trim()) { contentIndent = markerEnd + 1; firstLine = ''; }
  else if (spacing >= 5) { contentIndent = markerEnd + 1; firstLine = ' '.repeat(spacing - 1) + rest; }
  else { contentIndent = markerEnd + spacing; firstLine = rest; }
  if (contentIndent <= markerEnd) throw new Error(`a list item's content starts inside its marker at line ${start}`);
  const body = [firstLine];
  const open = { paragraph: false, fence: null };
  trackParagraph(open, firstLine);
  let i = start + 1;
  while (i < n) {
    const l = lines[i];
    if (isBlank(l)) {
      let j = i;
      while (j < n && isBlank(lines[j])) j += 1;
      // An item may start with one blank line, no more.
      if (j >= n || indentOf(lines[j]) < contentIndent || (body.length === 1 && body[0] === '' && j - i > 0)) break;
      for (; i < j; i += 1) body.push('');
      open.paragraph = false;
    } else if (indentOf(l) >= contentIndent) {
      body.push(l.slice(contentIndent));
      trackParagraph(open, body.at(-1));
      i += 1;
    } else if (open.paragraph && !LIST.test(l) && !startsBlock(l, depth)) {
      body.push(l);
      i += 1;
    } else break;
  }
  return { body, next: i, contentIndent };
}

// A list item; a leading `[ ]` or `[x]` before text makes it a task.
function taskItem(children) {
  const item = { c: children, task: null };
  const lead = children[0];
  if (lead?.t !== 'p') return item;
  const task = /^\[([ xX])\](?:[ \t]+|$)/.exec(lead.text);
  if (task && lead.text.length > task[0].length) {
    item.task = task[1] !== ' ';
    lead.text = lead.text.slice(task[0].length);
  }
  return item;
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
  return new InlineParser(String(src), refs).run();
}

const autolink = (href, text) => ({ t: 'link', href, title: null, image: false, depth: 1, inner: { t: 'text', v: text, prev: null, next: null } });

// Whether the delimiter run src[start, end) of `ch` can open or close emphasis.
function flanking(src, ch, start, end) {
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
}

// The CommonMark inline algorithm: nodes form a doubly linked chain, emphasis
// delimiters a linked stack (`delims` is its top), link openers an array.
class InlineParser {
  constructor(src, refs) {
    this.src = src;
    this.refs = refs;
    this.first = null;
    this.last = null;
    this.delims = null;
    this.brackets = [];
    // Link openers below this stack index are off: a link cannot hold a link.
    this.linkFloor = 0;
    this.buf = '';
    this.pos = 0;
    this.tickRuns = null;
    // Nesting scans share one budget, so flat input cannot make repeated
    // wrapping quadratic; once spent, nothing new nests.
    this.budget = 8 * src.length + 1000;
  }

  run() {
    const { src } = this;
    while (this.pos < src.length) {
      SPECIAL.lastIndex = this.pos;
      const hit = SPECIAL.exec(src);
      if (!hit) { this.buf += src.slice(this.pos); break; }
      this.buf += src.slice(this.pos, hit.index);
      this.pos = hit.index;
      const at = this.pos;
      this.step(src[at]);
      if (this.pos <= at) throw new Error(`the inline parser stalled at ${at}`);
    }
    this.flush();
    this.processEmphasis(null);
    return materialize(this.first, 0);
  }

  // Consumes the construct that starts with the special character `ch` at `pos`.
  step(ch) {
    const { src } = this;
    if (ch === '\\') this.backslash();
    else if (ch === '\n') this.lineEnd();
    else if (ch === '`') this.codeSpan();
    else if (ch === '*' || ch === '_' || ch === '~') this.delimiterRun(ch);
    else if (ch === '[' || (ch === '!' && src[this.pos + 1] === '[')) this.openBracket(ch === '!');
    else if (ch === '!') { this.buf += '!'; this.pos += 1; }
    else if (ch === ']') this.closeBracket();
    else if (ch === '<') this.angleAutolink();
    else if (ch === '&') this.entity();
    else this.bareUrl(ch);
  }

  append(node) {
    if (this.last ? this.last.next !== null : this.first !== null) throw new Error('the inline chain lost its tail');
    node.prev = this.last;
    node.next = null;
    if (this.last) this.last.next = node; else this.first = node;
    this.last = node;
    return node;
  }

  unlink(node) {
    if ((node.prev ? node.prev.next : this.first) !== node || (node.next ? node.next.prev : this.last) !== node) {
      throw new Error('unlinking a node that is not in the inline chain');
    }
    if (node.prev) node.prev.next = node.next; else this.first = node.next;
    if (node.next) node.next.prev = node.prev; else this.last = node.prev;
  }

  flush() {
    if (!this.buf) return;
    this.append({ t: 'text', v: this.buf });
    this.buf = '';
  }

  backslash() {
    const next = this.src[this.pos + 1];
    if (next === '\n') { this.flush(); this.append({ t: 'br' }); this.pos += 2; }
    else if (next !== undefined && ESCAPABLE.test(next)) { this.buf += next; this.pos += 2; }
    else { this.buf += '\\'; this.pos += 1; }
  }

  lineEnd() {
    // Count the spaces in the source, not the buffer: reading a long
    // buffer back would flatten it on every line.
    let spaces = 0;
    while (this.src.charCodeAt(this.pos - 1 - spaces) === 32 && spaces < this.buf.length) spaces += 1;
    if (spaces > 0) this.buf = this.buf.slice(0, this.buf.length - spaces);
    if (spaces >= 2) { this.flush(); this.append({ t: 'br' }); }
    else this.buf += '\n';
    this.pos += 1;
  }

  // Where the next run of exactly `len` backticks at or after `from` starts, or -1.
  closingTicks(len, from) {
    if (!this.tickRuns) {
      this.tickRuns = new Map();
      for (const m of this.src.matchAll(/`+/g)) {
        if (!this.tickRuns.has(m[0].length)) this.tickRuns.set(m[0].length, { at: [], k: 0 });
        this.tickRuns.get(m[0].length).at.push(m.index);
      }
    }
    const runs = this.tickRuns.get(len);
    if (!runs) return -1;
    while (runs.k < runs.at.length && runs.at[runs.k] < from) runs.k += 1;
    return runs.k < runs.at.length ? runs.at[runs.k] : -1;
  }

  codeSpan() {
    const { src } = this;
    let end = this.pos;
    while (src[end] === '`') end += 1;
    const len = end - this.pos;
    const close = this.closingTicks(len, end);
    if (close < 0) { this.buf += src.slice(this.pos, end); this.pos = end; return; }
    let code = src.slice(end, close).replace(/\n/g, ' ');
    if (code.length > 2 && code.startsWith(' ') && code.endsWith(' ') && /[^ ]/.test(code)) code = code.slice(1, -1);
    this.flush();
    this.append({ t: 'code', v: code });
    this.pos = close + len;
  }

  delimiterRun(ch) {
    const { src } = this;
    let end = this.pos;
    while (src[end] === ch) end += 1;
    const len = end - this.pos;
    const { canOpen, canClose } = flanking(src, ch, this.pos, end);
    if ((ch === '~' && len !== 2) || (!canOpen && !canClose)) { this.buf += src.slice(this.pos, end); this.pos = end; return; }
    this.flush();
    const node = this.append({ t: 'text', v: src.slice(this.pos, end) });
    const d = { ch, count: len, orig: len, node, canOpen, canClose, prev: this.delims, next: null };
    if (this.delims) this.delims.next = d;
    this.delims = d;
    this.pos = end;
  }

  openBracket(image) {
    this.flush();
    const node = this.append({ t: 'text', v: image ? '![' : '[' });
    this.pos += image ? 2 : 1;
    this.brackets.push({ node, at: this.pos, image, delim: this.delims });
  }

  angleAutolink() {
    const { src, pos } = this;
    AUTOLINK.lastIndex = pos;
    const am = AUTOLINK.exec(src);
    EMAIL.lastIndex = pos;
    const em = am ?? EMAIL.exec(src);
    const href = em && (am ? em[1] : `mailto:${em[1]}`);
    if (!href || !safeHref(href)) { this.buf += '<'; this.pos += 1; return; }
    this.flush();
    this.append(autolink(href, em[1]));
    // Like any link, an autolink stops the link openers around it from closing.
    this.linkFloor = this.brackets.length;
    this.pos += em[0].length;
  }

  entity() {
    ENTITY_AT.lastIndex = this.pos;
    const em = ENTITY_AT.exec(this.src);
    const text = em ? entityText(em[1]) : null;
    if (text === null) { this.buf += '&'; this.pos += 1; } else { this.buf += text; this.pos += em[0].length; }
  }

  // `h` or `w`: maybe a bare URL, at a word start and outside link text.
  bareUrl(ch) {
    const { src, pos } = this;
    const prev = src[pos - 1];
    let url = '';
    if (this.brackets.length === 0 && (prev === undefined || SPACE.test(prev) || '*_~('.includes(prev))) {
      URL_AT.lastIndex = pos;
      const um = URL_AT.exec(src);
      const text = um ? trimUrlTail(um[0]) : '';
      const www = /^www\./i.test(text);
      const host = /^[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*/.exec(www ? text.slice(4) : text.replace(/^https?:\/\//i, ''))?.[0] ?? '';
      if (host && (!www || host.includes('.')) && safeHref(www ? `http://${text}` : text)) url = text;
    }
    if (!url) { this.buf += ch; this.pos += 1; return; }
    this.flush();
    this.append(autolink(/^www\./i.test(url) ? `http://${url}` : url, url));
    this.pos += url.length;
  }

  // [text](dest "title"), from the "(" at `at`; returns { href, title, end } or null.
  parseTail(at) {
    const { src } = this;
    let p = at + 1;
    const ws = () => { while (p < src.length && /[ \t\n]/.test(src[p])) p += 1; };
    ws();
    const dest = this.destination(p);
    if (!dest) return null;
    const afterDest = dest.end;
    p = afterDest;
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
    return { href: unescapeText(dest.raw), title, end: p + 1 };
  }

  // A link destination at `from`, <bracketed> or bare with balanced parens; { raw, end } or null.
  destination(from) {
    const { src } = this;
    let p = from;
    if (src[p] === '<') {
      const start = from + 1;
      p = start;
      while (p < src.length && p - start <= MAX_URL_CHARS && src[p] !== '>' && src[p] !== '<' && src[p] !== '\n') p += src[p] === '\\' ? 2 : 1;
      if (src[p] !== '>') return null;
      return { raw: src.slice(start, p), end: p + 1 };
    }
    let depth = 0;
    while (p < src.length && p - from <= MAX_URL_CHARS && !/[\s\u0000-\u001f\u007f]/.test(src[p])) {
      if (src[p] === '\\' && p + 1 < src.length) { p += 2; continue; }
      if (src[p] === '(') depth += 1;
      else if (src[p] === ')') { if (depth === 0) break; depth -= 1; }
      p += 1;
    }
    if (depth !== 0 || p - from > MAX_URL_CHARS) return null;
    return { raw: src.slice(from, p), end: p };
  }

  // Nesting depth of the chain from `head` up to `stop`; Infinity once the budget is spent.
  depthOf(head, stop = null) {
    let d = 0;
    for (let n = head; n !== stop; n = n.next) {
      if (!n) throw new Error('the nesting scan left the chain before its stop');
      this.budget -= 1;
      if (this.budget < 0) return Infinity;
      d = Math.max(d, n.depth ?? 0);
    }
    return d;
  }

  // Wrap the nodes strictly between `from` and `to` in a new node; null when empty or too deep.
  wrapBetween(from, to, type) {
    if (from === to || !from.next) throw new Error('wrapBetween needs two distinct nodes in one chain');
    const head = from.next;
    if (head === to) return null;
    const tail = to.prev;
    const depth = this.depthOf(head, to) + 1;
    if (depth > MAX_INLINE_DEPTH) return null;
    const node = { t: type, depth, inner: head, prev: from, next: to };
    head.prev = null;
    tail.next = null;
    from.next = node;
    to.prev = node;
    return node;
  }

  removeDelim(d) {
    if (d.prev) d.prev.next = d.next;
    if (d.next) d.next.prev = d.prev; else this.delims = d.prev;
  }

  // The nearest opener below `closer` (down to `limit`) that it can pair with, or null.
  openerFor(closer, limit) {
    for (let opener = closer.prev; opener && opener !== limit; opener = opener.prev) {
      const odd = (closer.canOpen || opener.canClose) && closer.orig % 3 !== 0 && (opener.orig + closer.orig) % 3 === 0;
      if (opener.ch === closer.ch && opener.canOpen && !odd && (closer.ch !== '~' || opener.count === closer.count)) return opener;
    }
    return null;
  }

  // Wraps the text between `opener` and `closer` in em, strong or del; returns the closer to try next.
  pair(opener, closer) {
    const use = closer.ch === '~' ? 2 : (closer.count >= 2 && opener.count >= 2 ? 2 : 1);
    opener.count -= use;
    closer.count -= use;
    if (opener.count < 0 || closer.count < 0) throw new Error('an emphasis pair used more delimiters than it had');
    opener.node.v = opener.node.v.slice(0, opener.node.v.length - use);
    closer.node.v = closer.node.v.slice(use);
    this.wrapBetween(opener.node, closer.node, closer.ch === '~' ? 'del' : (use === 2 ? 'strong' : 'em'));
    // Drop the delimiters between the pair.
    opener.next = closer;
    closer.prev = opener;
    if (opener.count === 0) { this.unlink(opener.node); this.removeDelim(opener); }
    if (closer.count > 0) return closer;
    const next = closer.next;
    this.unlink(closer.node);
    this.removeDelim(closer);
    return next;
  }

  processEmphasis(bottom) {
    const bottoms = new Map();
    let closer = bottom ? bottom.next : this.delims;
    while (!bottom && closer?.prev) closer = closer.prev;
    while (closer) {
      if (!closer.canClose) { closer = closer.next; continue; }
      const key = `${closer.ch}${closer.orig % 3}${closer.canOpen ? 1 : 0}`;
      const opener = this.openerFor(closer, bottoms.has(key) ? bottoms.get(key) : bottom);
      if (opener) { closer = this.pair(opener, closer); continue; }
      const old = closer;
      closer = closer.next;
      bottoms.set(key, old.prev);
      if (!old.canOpen) this.removeDelim(old);
    }
    // Everything above the bottom is spent.
    if (bottom) { bottom.next = null; this.delims = bottom; } else this.delims = null;
  }

  closeBracket() {
    const opener = this.brackets.pop();
    this.pos += 1;
    const off = opener && !opener.image && this.brackets.length < this.linkFloor;
    if (this.linkFloor > this.brackets.length) this.linkFloor = this.brackets.length;
    const found = opener && !off ? this.linkTarget(opener) : null;
    if (!found) { this.buf += ']'; return; }
    this.flush();
    this.processEmphasis(opener.delim);
    this.append(this.takeLabel(opener, found.target));
    // Links do not nest: earlier link openers can no longer close.
    if (!opener.image) this.linkFloor = this.brackets.length;
    this.pos = found.end;
  }

  // The link the "]" just before `pos` closes: an inline (dest) or a reference.
  linkTarget(opener) {
    const { src, pos } = this;
    if (src[pos] === '(') {
      const tail = this.parseTail(pos);
      if (tail) return { target: tail, end: tail.end };
    }
    if (this.refs.size === 0 || pos - 1 - opener.at > 999) return null;
    let label = src.slice(opener.at, pos - 1);
    let end = pos;
    if (src[end] === '[') {
      const close = src.indexOf(']', end + 1);
      if (close > 0 && close - end <= 1000 && !src.slice(end + 1, close).includes('[')) {
        if (close > end + 1) label = src.slice(end + 1, close);
        end = close + 1;
      }
    }
    const ref = label.trim() ? this.refs.get(normalizeLabel(label)) : null;
    return ref ? { target: ref, end } : null;
  }

  // Detaches everything after the opener's "[" as the label of a new link node.
  takeLabel(opener, target) {
    const label = opener.node;
    const inner = label.next;
    this.last = label.prev;
    if (this.last) this.last.next = null; else this.first = null;
    const link = { t: 'link', href: target.href, title: target.title, image: opener.image, inner, depth: 1 };
    if (!inner) return link;
    inner.prev = null;
    link.depth = this.depthOf(inner) + 1;
    // Pathologically nested labels keep the link but drop the label.
    if (link.depth > MAX_INLINE_DEPTH) { link.inner = null; link.depth = 1; }
    return link;
  }
}


// Linked chain to arrays, merging neighbouring text. Images become links to
// their source; inside a link, links and images flatten to text, as anchors cannot nest.
function materialize(head, depth, inLink = false) {
  if (depth > MAX_INLINE_DEPTH) throw new RangeError(`inline nesting ${depth} is past the cap`);
  const out = [];
  const text = (v) => {
    if (!v) return;
    if (out.at(-1)?.t === 'text') out.at(-1).v += v; else out.push({ t: 'text', v });
  };
  for (let n = head; n; n = n.next) {
    if (n.t === 'text') text(n.v);
    else if (n.t === 'code') out.push({ t: 'code', v: n.v });
    else if (n.t === 'br') out.push({ t: 'br' });
    else if (n.t !== 'link') out.push({ t: n.t, c: materialize(n.inner, depth + 1, inLink) });
    else {
      const c = materialize(n.inner, depth + 1, true);
      const label = n.image ? [{ t: 'text', v: plainText(c) || 'image' }] : c;
      if (inLink) for (const node of label) { if (node.t === 'text') text(node.v); else out.push(node); }
      else out.push(linkNode(n, label));
    }
  }
  return out;
}

function linkNode(n, c) {
  const safe = safeHref(n.href);
  return { t: 'link', href: safe?.href ?? null, external: safe?.external ?? false, title: n.title ?? null, image: Boolean(n.image), c };
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
  const lines = text.replace(/\r\n?/g, '\n').replace(/\u0000/g, '�').split('\n').map((line) => expandTabs(line));
  const ctx = { refs: new Map() };
  return inlineAll(parseBlocks(lines, 0, ctx), ctx.refs);
}
