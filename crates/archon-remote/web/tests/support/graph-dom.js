// The DOM shim with what the graph renderer needs on top: event dispatch with
// capture and bubbling, pointer capture, layout reads, resizes and frames.
import { installDom, Element } from './dom-shim.js';

export class GraphElement extends Element {
  handlers = new Map();
  captures = new Set();
  addEventListener(type, handler, options) {
    const list = this.handlers.get(type) ?? [];
    list.push({ handler, capture: options === true });
    this.handlers.set(type, list);
  }
  removeEventListener(type, handler) {
    this.handlers.set(type, (this.handlers.get(type) ?? []).filter((entry) => entry.handler !== handler));
  }
  append(...nodes) {
    for (const item of nodes) {
      if (item.tag === '#fragment') { this.append(...item.children); continue; }
      item.parent = this;
      this.children.push(item);
    }
  }
  remove() {
    if (!this.parent) return;
    this.parent.children = this.parent.children.filter((item) => item !== this);
    this.parent = null;
  }
  closest(selector) {
    if (selector === '.graph-node' && this.className.split(' ').includes('graph-node')) return this;
    return this.parent?.closest(selector) ?? null;
  }
  scrollIntoView() {}
  focus() { document.activeElement = this; this.dispatch('focus', { bubbles: false }); }
  get ownerSVGElement() { return this.tag === 'svg' ? this : this.parent?.ownerSVGElement; }
  setPointerCapture(id) { this.captures.add(id); }
  hasPointerCapture(id) { return this.captures.has(id); }
  releasePointerCapture(id) { this.captures.delete(id); }
  getBoundingClientRect() { this.reads = (this.reads ?? 0) + 1; return { left: 10, top: 20, width: 360, height: 400 }; }
  dispatch(type, properties = {}) {
    const event = { type, target: this, bubbles: true, prevented: false, stopped: false,
      preventDefault() { this.prevented = true; }, stopPropagation() { this.stopped = true; },
      stopImmediatePropagation() { this.stopped = true; this.immediate = true; }, ...properties };
    const path = [this];
    while (path.at(-1).parent) path.push(path.at(-1).parent);
    for (const capture of [true, false]) {
      for (const item of capture ? [...path].reverse() : path) {
        for (const entry of item.handlers?.get(type) ?? []) {
          if (entry.capture === capture) entry.handler(event);
          if (event.immediate) break;
        }
        if (event.stopped || (!capture && !event.bubbles)) break;
      }
      if (event.stopped) break;
    }
    return event;
  }
}

export function domHarness() {
  installDom();
  document.createElement = (tag) => new GraphElement(tag);
  document.createElementNS = (_ns, tag) => new GraphElement(tag);
  document.createDocumentFragment = () => new GraphElement('#fragment');
  const observers = [];
  const frames = new Map();
  let nextFrame = 0;
  globalThis.ResizeObserver = class {
    constructor(callback) { this.callback = callback; observers.push(this); }
    observe(element) { this.element = element; }
    disconnect() { this.disconnected = true; }
  };
  globalThis.requestAnimationFrame = (callback) => { frames.set(++nextFrame, callback); return nextFrame; };
  globalThis.cancelAnimationFrame = (id) => frames.delete(id);
  return { observers, frames, flush: () => {
    const pending = [...frames.values()];
    frames.clear();
    for (const callback of pending) callback();
  }, resize: () => {
    for (const observer of observers) observer.callback([{ contentRect: { width: 360, height: 400 } }]);
  } };
}
