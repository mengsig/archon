// Pairing codes are 8 Crockford base32 symbols. Case, spaces and dashes are
// ignored, and the look-alikes O, I and L read as 0, 1 and 1.
export const PAIRING_CODE_LENGTH = 8;
const CROCKFORD = /^[0-9A-HJKMNP-TV-Z]*$/;
const LOOK_ALIKES = { O: '0', I: '1', L: '1' };

export function normalizePairingCode(input) {
  if (typeof input !== 'string') throw new TypeError('a pairing code is a string');
  return input
    .toUpperCase()
    .replace(/[\s-]+/g, '')
    .replace(/[OIL]/g, (c) => LOOK_ALIKES[c]);
}

// Why a typed code cannot be sent yet, or null when it is complete.
export function pairingCodeProblem(input) {
  const code = normalizePairingCode(input);
  if (!CROCKFORD.test(code)) return 'Use only the letters and digits shown on the host.';
  if (code.length < PAIRING_CODE_LENGTH) return `Enter all ${PAIRING_CODE_LENGTH} characters.`;
  if (code.length > PAIRING_CODE_LENGTH) return `The code has ${PAIRING_CODE_LENGTH} characters.`;
  return null;
}

// "ABCDEFGH" as "ABCD-EFGH"; a partial code is grouped as far as it goes.
export function formatPairingCode(input) {
  const code = normalizePairingCode(input).slice(0, PAIRING_CODE_LENGTH);
  return code.length > 4 ? `${code.slice(0, 4)}-${code.slice(4)}` : code;
}

// The code a pairing link carries in its fragment: `#code=ABCDEFGH`.
export function codeFromHash(hash) {
  if (typeof hash !== 'string' || !hash.startsWith('#')) return null;
  const value = new URLSearchParams(hash.slice(1)).get('code');
  return value ? normalizePairingCode(value).slice(0, 16) : null;
}
