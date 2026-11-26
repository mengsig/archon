import { h } from '../../ui/dom.js';
import { openSheet } from '../../ui/sheet.js';
import { toast } from '../../ui/toast.js';
import { fileLimitMessage, formatSize, uploadFiles } from './files.js';

const button = (label, onclick) => h('button', { class: 'button button-plain button-block', type: 'button', onclick }, label);

export function showFileChooser(design, session, event, refuse) {
  let answered = false;
  const input = h('input', { type: 'file', hidden: true, accept: event.accept, multiple: event.multiple });
  const progress = h('p', { role: 'status' });
  const choose = button('Choose file…', () => input.click());
  const sheet = openSheet({ title: 'The app asks for a file', content: h('div', { class: 'confirm' }, input, progress, choose, button('Cancel', () => sheet.close())), onClose: () => {
    if (!answered) { answered = true; design.fileSet(session, event.id, null).catch(refuse); }
  } });
  input.addEventListener('cancel', () => sheet.close());
  input.addEventListener('change', async () => {
    const files = Array.from(input.files);
    if (!files.length) { sheet.close(); return; }
    const problem = fileLimitMessage(files);
    if (problem) { toast(problem, { tone: 'amber' }); input.value = ''; return; }
    choose.disabled = true;
    try {
      const transfer = await uploadFiles(design, session, files, (name, percent) => { progress.textContent = `Sending ${name}… ${percent}%`; });
      if (sheet.closed) return;
      answered = true;
      await design.fileSet(session, event.id, transfer);
      sheet.close();
    } catch (error) { answered = false; refuse(error); choose.disabled = false; }
  });
}

export function showDownload(design, session, event, refuse) {
  const status = h('p', { role: 'status' });
  const save = button('Save', async () => {
    save.disabled = true;
    try { await saveDownload(design, session, event, status); sheet.close(); }
    catch (error) { refuse(error); save.disabled = false; }
  });
  const sheet = openSheet({ title: `The app downloaded ${event.name} (${formatSize(event.size)})`, content: h('div', { class: 'confirm' }, status, save) });
}

async function saveDownload(design, session, event, status) {
  const chunks = [];
  let offset = 0;
  while (offset < event.size) {
    const part = await design.download(session, event.id, offset);
    if (part.id !== event.id || part.offset !== offset || part.size !== event.size) throw new Error('The download response did not match the requested file.');
    const binary = atob(part.data);
    if (!binary.length || offset + binary.length > event.size) throw new Error('The download returned an invalid chunk.');
    chunks.push(Uint8Array.from(binary, (char) => char.charCodeAt(0)));
    offset += binary.length;
    status.textContent = `Saving… ${Math.round(offset / event.size * 100)}%`;
  }
  const url = URL.createObjectURL(new Blob(chunks));
  try { const link = h('a', { href: url, download: event.name }); document.body.append(link); link.click(); link.remove(); }
  finally { URL.revokeObjectURL(url); }
}

export function showPicker(design, session, event, refuse) {
  let answered = false;
  const answer = (value) => { answered = true; design.pickerAnswer(session, event.id, value).catch(refuse); sheet.close(); };
  let content;
  if (event.control === 'select') content = pickerOptions(event.options, answer);
  else {
    const input = h('input', { class: 'input', type: event.control, value: event.value, min: event.min, max: event.max, step: event.step });
    content = h('div', { class: 'confirm' }, input, button('OK', () => { if (input.reportValidity()) answer(input.value); }), button('Cancel', () => sheet.close()));
  }
  const sheet = openSheet({ title: 'Choose for the app', content, onClose: () => { if (!answered) design.pickerAnswer(session, event.id, null).catch(refuse); } });
}

function pickerOptions(options, answer) {
  const rows = [];
  let group = '';
  for (const option of options) {
    if (option.group && option.group !== group) rows.push(h('h3', { text: option.group }));
    group = option.group;
    const row = button(`${option.label}${option.selected ? ' ✓' : ''}`, () => answer(option.value));
    row.disabled = option.disabled;
    rows.push(row);
  }
  return h('div', { class: 'confirm' }, rows);
}

// The page can report any text, so it reaches the clipboard only when the operator presses Copy.
export function showClipboard(text) {
  clipboardSheet(text);
}

function clipboardSheet(text) {
  const input = h('textarea', { class: 'input', readonly: true, 'aria-label': 'Copied text' });
  input.value = text;
  const copy = button('Copy', async () => {
    try {
      if (navigator.clipboard?.writeText) await navigator.clipboard.writeText(text);
      else { input.select(); if (!document.execCommand('copy')) throw new Error('Copy failed'); }
      toast('Copied from the app');
      sheet.close();
    } catch (error) {
      input.select();
      try {
        if (!document.execCommand('copy')) throw new Error('Copy failed');
        toast('Copied from the app');
        sheet.close();
      } catch (copyError) { toast('Select the text to copy it.', { tone: 'amber' }); }
    }
  });
  const sheet = openSheet({ title: 'The app copied text', content: h('div', { class: 'confirm' }, input, copy) });
  requestAnimationFrame(() => { input.focus(); input.select(); });
}
