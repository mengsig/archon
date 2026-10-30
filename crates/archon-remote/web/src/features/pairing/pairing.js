// Pairing: the one-time code from the host and a name for this device.
import * as api from '../../core/api.js';
import { formatPairingCode, normalizePairingCode, pairingCodeProblem } from '../../core/pairing-code.js';
import { suggestDeviceName, deviceNameProblem, MAX_DEVICE_NAME } from '../../core/device-name.js';
import { h } from '../../ui/dom.js';
import { field } from '../../ui/components.js';

const CLAIM_ERRORS = {
  invalid_code: 'That code did not match. Check it and try again.',
  expired: 'That code has expired. Start pairing again on the host for a new one.',
  pairing_closed: 'Pairing is not open on the host. Start it there, then enter the new code.',
  bad_request: null,
};

export function showPairing(root, { code = null, notice = null, onPaired }) {
  if (typeof onPaired !== 'function') throw new TypeError('pairing needs onPaired');
  const codeInput = h('input', {
    class: 'input input-code', name: 'code', type: 'text', inputmode: 'text', autocomplete: 'one-time-code',
    autocapitalize: 'characters', autocorrect: 'off', spellcheck: 'false', maxlength: '11',
    placeholder: 'XXXX-XXXX', value: code ? formatPairingCode(code) : '', 'aria-describedby': 'pair-error',
  });
  const nameInput = h('input', {
    class: 'input', name: 'device_name', type: 'text', autocomplete: 'off', maxlength: String(MAX_DEVICE_NAME),
    value: suggestDeviceName(navigator.userAgent),
  });
  const error = h('p', { class: 'form-error', id: 'pair-error', role: 'alert' });
  const status = h('p', { class: 'pair-status', role: 'status' });
  const submit = h('button', { class: 'button button-primary button-block', type: 'submit' }, 'Pair');
  const form = h('form', { class: 'pair-form', novalidate: true, 'aria-label': 'Pair this device' },
    field({ label: 'Pairing code', control: codeInput }),
    field({ label: 'Device name', control: nameInput, hint: 'Shown on the host, so you can tell your devices apart.' }),
    error, submit);

  codeInput.addEventListener('input', () => {
    const atEnd = codeInput.selectionStart === codeInput.value.length;
    const formatted = formatPairingCode(codeInput.value);
    if (formatted !== codeInput.value && atEnd) codeInput.value = formatted;
    error.textContent = '';
  });
  form.addEventListener('submit', (event) => {
    event.preventDefault();
    claim({ codeInput, nameInput, error, submit, onPaired });
  });

  root.replaceChildren(h('div', { class: 'pairing' },
    h('div', { class: 'pair-mark', 'aria-hidden': 'true' }, h('img', { src: '/icons/icon.svg', alt: '', width: '72', height: '72' })),
    h('h1', { class: 'pair-title', text: 'Pair this device' }),
    h('p', { class: 'pair-lead', text: 'Pairing lets this device watch and steer Archon on your host. Enter the code shown in the host’s terminal, or scan its QR code.' }),
    notice ? h('p', { class: 'callout', role: 'status', text: notice }) : null,
    status, form));
  showPairingState(status);
  (code ? nameInput : codeInput).focus({ preventScroll: true });
}

async function showPairingState(status) {
  try {
    const health = await api.health();
    if (health.pairing_open === false) status.textContent = 'Pairing is not open on the host right now.';
  } catch (err) {
    status.textContent = `The host did not answer: ${err.message}`;
  }
}

async function claim({ codeInput, nameInput, error, submit, onPaired }) {
  const problem = pairingCodeProblem(codeInput.value) ?? deviceNameProblem(nameInput.value);
  if (problem) {
    error.textContent = problem;
    (pairingCodeProblem(codeInput.value) ? codeInput : nameInput).focus();
    return;
  }
  submit.disabled = true;
  submit.textContent = 'Pairing…';
  error.textContent = '';
  try {
    await api.claim(normalizePairingCode(codeInput.value), nameInput.value.trim());
    onPaired();
  } catch (err) {
    error.textContent = claimMessage(err);
    submit.disabled = false;
    submit.textContent = 'Pair';
  }
}

function claimMessage(err) {
  if (err.code === 'rate_limited') {
    const wait = err.retryAfterSecs ? ` Try again in ${err.retryAfterSecs} s.` : ' Wait a moment and try again.';
    return `Too many attempts.${wait}`;
  }
  return CLAIM_ERRORS[err.code] ?? err.message;
}
