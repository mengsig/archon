export const CHUNK_SIZE = 512 * 1024;
const FILE_LIMIT = 64 * 1024 * 1024;
const TOTAL_LIMIT = 256 * 1024 * 1024;

export function fileLimitMessage(files) {
  if (files.length > 32) return 'Choose at most 32 files for the preview.';
  if (files.some((file) => file.size > FILE_LIMIT)) return 'That file is too big for the preview (64 MiB at most).';
  if (files.reduce((sum, file) => sum + file.size, 0) > TOTAL_LIMIT) return 'Those files are too big for the preview (256 MiB total at most).';
  return null;
}

// Mirrors crates/archon-daemon/src/design/accept.rs. The daemon is the authority: this check
// only refuses early with a clear message and must never be stricter than the daemon.
const MIME_BY_EXTENSION = new Map(Object.entries({
  png: 'image/png',
  jpg: 'image/jpeg',
  jpeg: 'image/jpeg',
  gif: 'image/gif',
  webp: 'image/webp',
  svg: 'image/svg+xml',
  bmp: 'image/bmp',
  ico: 'image/x-icon',
  avif: 'image/avif',
  tif: 'image/tiff',
  tiff: 'image/tiff',
  heic: 'image/heic',
  txt: 'text/plain',
  csv: 'text/csv',
  tsv: 'text/tab-separated-values',
  json: 'application/json',
  xml: 'application/xml',
  html: 'text/html',
  md: 'text/markdown',
  pdf: 'application/pdf',
  rtf: 'application/rtf',
  doc: 'application/msword',
  xls: 'application/vnd.ms-excel',
  ppt: 'application/vnd.ms-powerpoint',
  docx: 'application/vnd.openxmlformats-officedocument.wordprocessingml.document',
  xlsx: 'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet',
  pptx: 'application/vnd.openxmlformats-officedocument.presentationml.presentation',
  odt: 'application/vnd.oasis.opendocument.text',
  ods: 'application/vnd.oasis.opendocument.spreadsheet',
  zip: 'application/zip',
  gz: 'application/gzip',
  tar: 'application/x-tar',
  mp3: 'audio/mpeg',
  wav: 'audio/wav',
  ogg: 'audio/ogg',
  mp4: 'video/mp4',
  webm: 'video/webm',
  mov: 'video/quicktime',
}));

const MAX_FILE_NAME_BYTES = 120;
const RUST_WHITESPACE = '\\t\\n\\v\\f\\r \\u0085\\u00a0\\u1680\\u2000-\\u200a\\u2028\\u2029\\u202f\\u205f\\u3000';
const RUST_TRIM_START = new RegExp(`^[${RUST_WHITESPACE}]+`);
const RUST_TRIM_END = new RegExp(`[${RUST_WHITESPACE}]+$`);

// Rust's str::trim; String.prototype.trim also strips U+FEFF, which Rust keeps.
const rustTrim = (text) => text.replace(RUST_TRIM_START, '').replace(RUST_TRIM_END, '');

const asciiLower = (text) => text.replace(/[A-Z]/g, (c) => c.toLowerCase());

const utf8Length = (c) => c.codePointAt(0) < 0x80 ? 1 : c.codePointAt(0) < 0x800 ? 2 : c.codePointAt(0) < 0x10000 ? 3 : 4;
const isControl = (c) => /^[\u0000-\u001f\u007f-\u009f]$/.test(c);

// Mirrors archon-design-browser sanitize::file_name, which the daemon applies before matching.
function sanitizedName(raw) {
  const last = String(raw ?? '').split(/[/\\]/).pop();
  let name = '';
  let bytes = 0;
  for (const c of last) {
    if (isControl(c)) continue;
    const kept = '<>:"|?*'.includes(c) ? '_' : c;
    const width = utf8Length(kept);
    if (bytes + width > MAX_FILE_NAME_BYTES) break;
    name += kept;
    bytes += width;
  }
  const trimmed = rustTrim(rustTrim(name).replace(/^\.+/, ''));
  return trimmed || 'file';
}

function mimeOf(name) {
  const dot = name.lastIndexOf('.');
  if (dot <= 0) return null;
  return MIME_BY_EXTENSION.get(asciiLower(name.slice(dot + 1))) ?? null;
}

function declaredType(raw) {
  const bare = rustTrim(String(raw ?? '').split(';')[0]);
  const parts = bare.split('/');
  const isToken = (s) => /^[\x21-\x7e]+$/.test(s);
  const valid = bare.length <= 255 && parts.length === 2 && isToken(parts[0]) && isToken(parts[1]);
  return valid ? asciiLower(bare) : '';
}

function mimeMatches(token, mime) {
  return token.endsWith('/*') ? mime.split('/')[0] === token.slice(0, -2) : mime === token;
}

function tokenAllows(token, file) {
  const fileName = sanitizedName(file.name);
  if (token.startsWith('.')) {
    const name = asciiLower(fileName);
    return name.length > token.length && name.endsWith(token);
  }
  const mimes = [declaredType(file.type), mimeOf(fileName)].filter(Boolean);
  return mimes.some((mime) => mimeMatches(token, mime));
}

// The input's accept attribute applies to what the operator picks or drops.
export function acceptMessage(accept, files) {
  const tokens = String(accept ?? '').split(',').map((t) => asciiLower(rustTrim(t))).filter(Boolean);
  if (!tokens.length || files.every((file) => tokens.some((token) => tokenAllows(token, file)))) return null;
  return `That file type is not one the app accepts here (${tokens.join(', ')}).`;
}

export function chunkOffsets(size) {
  if (!Number.isSafeInteger(size) || size < 0 || size > FILE_LIMIT) throw new RangeError('invalid file size');
  return Array.from({ length: Math.max(1, Math.ceil(size / CHUNK_SIZE)) }, (_, index) => index * CHUNK_SIZE);
}

export function transferId() {
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  return `t${Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('')}`;
}

export function formatSize(size) {
  if (!Number.isSafeInteger(size) || size < 0) throw new RangeError('invalid download size');
  if (size < 1024) return `${size} B`;
  const units = ['KiB', 'MiB', 'GiB', 'TiB'];
  let value = size / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) { value /= 1024; unit++; }
  return `${Number(value.toFixed(1))} ${units[unit]}`;
}

function encode(bytes) {
  let binary = '';
  for (let i = 0; i < bytes.length; i += 8192) binary += String.fromCharCode(...bytes.subarray(i, i + 8192));
  return btoa(binary);
}

export async function uploadFiles(design, session, files, progress = () => {}) {
  const problem = fileLimitMessage(files);
  if (problem || !files.length) throw new Error(problem ?? 'Choose at least one file.');
  const transfer = transferId();
  for (const [file, entry] of files.entries()) {
    for (const offset of chunkOffsets(entry.size)) {
      const bytes = new Uint8Array(await entry.slice(offset, offset + CHUNK_SIZE).arrayBuffer());
      await design.filePart({ session, transfer, file, name: entry.name, size: entry.size, media_type: entry.type || undefined, offset, data: encode(bytes) });
      progress(entry.name, entry.size ? Math.round((offset + bytes.length) / entry.size * 100) : 100);
    }
  }
  return transfer;
}
