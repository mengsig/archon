// Installed in the preview's main world before the app's own scripts. It turns what a
// headless browser cannot show (popups, new tabs, native pickers, the clipboard) into
// messages for Archon. Every message is untrusted: archond validates and caps it.
(() => {
  const send = globalThis.__archonDesignBridge;
  if (typeof send !== 'function' || window.top !== window || window.__archonDesign) return;
  const post = (message) => send(JSON.stringify(message));
  const SAME_TAB = /^(_self|_top|_parent)?$/i;
  const PICKERS = new Set(['date', 'time', 'datetime-local', 'month', 'week', 'color']);
  const pending = new Map();
  let next = 1;

  const openHere = (url) => {
    const href = new URL(String(url), location.href).href;
    post({ kind: 'tab', url: href });
    location.assign(href);
  };
  const nativeOpen = window.open;
  window.open = function open(url, target, features) {
    if (target !== undefined && SAME_TAB.test(String(target)) && String(target) !== '') {
      return nativeOpen.call(window, url, target, features);
    }
    if (url === undefined || String(url) === '' || String(url) === 'about:blank') {
      post({ kind: 'tab', url: '' });
      return null;
    }
    openHere(url);
    return window;
  };

  const linkOf = (event) => event.target instanceof Element ? event.target.closest('a[href], area[href]') : null;
  const wantsNewTab = (event, link) => !SAME_TAB.test(link.getAttribute('target') ?? '')
    || event.ctrlKey || event.metaKey || event.shiftKey || event.button === 1;
  const onLink = (event) => {
    const link = linkOf(event);
    if (!link || event.defaultPrevented || link.hasAttribute('download') || !wantsNewTab(event, link)) return;
    event.preventDefault();
    openHere(link.href);
  };
  window.addEventListener('click', onLink);
  window.addEventListener('auxclick', onLink);
  window.addEventListener('submit', (event) => {
    const form = event.target;
    if (event.defaultPrevented || !(form instanceof HTMLFormElement)) return;
    if (!SAME_TAB.test(form.getAttribute('target') ?? '')) form.target = '_self';
    const by = event.submitter;
    if (by && !SAME_TAB.test(by.getAttribute('formtarget') ?? '')) by.formTarget = '_self';
  });

  const optionsOf = (select) => Array.from(select.options).slice(0, 500).map((option) => {
    const group = option.parentElement instanceof HTMLOptGroupElement ? option.parentElement : null;
    return {
      label: option.label || option.text, value: String(option.index), selected: option.selected,
      disabled: option.disabled || Boolean(group?.disabled), group: group?.label ?? '',
    };
  });
  const openPicker = (control) => {
    const id = next++;
    pending.clear();
    pending.set(id, control);
    if (control instanceof HTMLSelectElement) {
      post({ kind: 'picker', id, control: 'select', value: String(control.selectedIndex), options: optionsOf(control) });
      return;
    }
    post({ kind: 'picker', id, control: control.type, value: control.value, min: control.min, max: control.max, step: control.step });
  };
  const pickable = (target) => {
    const control = target instanceof Element ? target.closest('select, input') : null;
    if (!control || control.disabled) return null;
    if (control instanceof HTMLSelectElement) return control.multiple || control.size > 1 ? null : control;
    return PICKERS.has(control.type) && !control.readOnly ? control : null;
  };
  document.addEventListener('mousedown', (event) => {
    const control = event.button === 0 ? pickable(event.target) : null;
    if (!control) return;
    event.preventDefault();
    control.focus();
    openPicker(control);
  }, true);
  document.addEventListener('keydown', (event) => {
    const control = pickable(event.target);
    const opens = event.key === ' ' || event.key === 'Enter' || event.key === 'F4' || (event.altKey && event.key === 'ArrowDown');
    if (!control || !opens || (event.key === 'Enter' && !(control instanceof HTMLSelectElement))) return;
    event.preventDefault();
    openPicker(control);
  }, true);
  for (const proto of [HTMLSelectElement.prototype, HTMLInputElement.prototype]) {
    const native = proto.showPicker;
    if (typeof native !== 'function') continue;
    proto.showPicker = function showPicker() {
      return pickable(this) === this ? openPicker(this) : native.call(this);
    };
  }

  const setValue = (control, value) => {
    const proto = control instanceof HTMLSelectElement ? HTMLSelectElement.prototype : HTMLInputElement.prototype;
    Object.getOwnPropertyDescriptor(proto, 'value').set.call(control, value);
  };
  const answer = (id, value) => {
    const control = pending.get(id);
    pending.delete(id);
    if (!control || !control.isConnected || value === null) return false;
    if (control instanceof HTMLSelectElement) {
      const option = control.options[Number(value)];
      if (!option || option.disabled || option.selected) return Boolean(option?.selected);
      setValue(control, option.value);
      control.selectedIndex = option.index;
    } else {
      setValue(control, String(value));
    }
    control.dispatchEvent(new Event('input', { bubbles: true }));
    control.dispatchEvent(new Event('change', { bubbles: true }));
    return true;
  };

  const selectedText = () => {
    const field = document.activeElement;
    if ((field instanceof HTMLInputElement || field instanceof HTMLTextAreaElement) && field.selectionStart !== null) {
      return field.value.slice(field.selectionStart, field.selectionEnd);
    }
    return String(window.getSelection() ?? '');
  };
  const onCopy = (event) => {
    const set = event.defaultPrevented ? event.clipboardData?.getData('text/plain') : null;
    const text = set ?? selectedText();
    if (text) post({ kind: 'copy', text: text.slice(0, 65536) });
  };
  window.addEventListener('copy', onCopy);
  window.addEventListener('cut', onCopy);
  if (navigator.clipboard) {
    navigator.clipboard.writeText = (text) => {
      post({ kind: 'copy', text: String(text).slice(0, 65536) });
      return Promise.resolve();
    };
  }

  Object.defineProperty(window, '__archonDesign', { value: Object.freeze({ answer }) });
})();
