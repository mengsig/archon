// Safe DOM building: text is always text, never markup. Styles come from
// classes (the CSP forbids style attributes); handlers from `on*` props.
export function h(tag, props = {}, ...children) {
  if (typeof tag !== 'string' || !tag) throw new TypeError('h() needs a tag name');
  const el = document.createElement(tag);
  setProps(el, props ?? {});
  append(el, children);
  return el;
}

function setProps(el, props) {
  for (const [key, value] of Object.entries(props)) {
    if (value === null || value === undefined || value === false) continue;
    if (key === 'style' || key === 'innerHTML') throw new Error(`h() refuses ${key}; use classes and text`);
    if (key === 'class') el.className = Array.isArray(value) ? value.filter(Boolean).join(' ') : value;
    else if (key === 'text') el.textContent = String(value);
    else if (key === 'dataset') Object.assign(el.dataset, value);
    else if (key.startsWith('on') && typeof value === 'function') el.addEventListener(key.slice(2), value);
    else if (key === 'value' || key === 'checked' || key === 'disabled' || key === 'selected') el[key] = value;
    else el.setAttribute(key, value === true ? '' : String(value));
  }
}

export function append(el, children) {
  for (const child of children.flat(Infinity)) {
    if (child === null || child === undefined || child === false) continue;
    el.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return el;
}

export function clear(el) {
  el.replaceChildren();
  return el;
}

export function replace(el, ...children) {
  el.replaceChildren();
  return append(el, children);
}
