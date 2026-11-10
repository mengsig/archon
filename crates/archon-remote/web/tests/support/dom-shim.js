// A tiny DOM for the unit tests: enough of Node, Element and Text for h(),
// with an HTML serializer so renders can be asserted as strings.
class Node {}
class Text extends Node {
  constructor(data) { super(); this.data = data; this.parent = null; }
  get textContent() { return this.data; }
}
class Element extends Node {
  constructor(tag) {
    super();
    this.tag = tag;
    this.attrs = new Map();
    this.children = [];
    this.parent = null;
    this.dataset = {};
    this.listeners = {};
    const self = this;
    this.classList = { add(c) { self.className = [self.className, c].filter(Boolean).join(' '); }, toggle() {} };
  }
  set className(v) { this.attrs.set('class', v); }
  get className() { return this.attrs.get('class') ?? ''; }
  setAttribute(k, v) { this.attrs.set(k, String(v)); }
  getAttribute(k) { return this.attrs.has(k) ? this.attrs.get(k) : null; }
  set disabled(v) { if (v) this.attrs.set('disabled', ''); }
  set checked(v) { if (v) this.attrs.set('checked', ''); }
  addEventListener(type, fn) { this.listeners[type] = fn; }
  append(...nodes) { for (const n of nodes) { n.parent = this; this.children.push(n); } }
  replaceChildren(...nodes) { this.children = []; this.append(...nodes); }
  set textContent(v) { this.children = []; this.append(new Text(String(v))); }
  get textContent() { return this.children.map((c) => c.textContent).join(''); }
  query(tag, out = []) {
    for (const c of this.children) if (c instanceof Element) { if (c.tag === tag) out.push(c); c.query(tag, out); }
    return out;
  }
}

const VOID = new Set(['br', 'hr', 'input']);
const esc = (s) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');

export function html(node) {
  if (node instanceof Text) return esc(node.data);
  const attrs = [...node.attrs].map(([k, v]) => (v === '' ? ` ${k}` : ` ${k}="${esc(v)}"`)).join('');
  return VOID.has(node.tag) ? `<${node.tag}${attrs}>` : `<${node.tag}${attrs}>${node.children.map(html).join('')}</${node.tag}>`;
}

export function installDom() {
  globalThis.Node = Node;
  globalThis.document = { createElement: (tag) => new Element(tag), createTextNode: (data) => new Text(data) };
}
export { Element };
