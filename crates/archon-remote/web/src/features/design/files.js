export const CHUNK_SIZE = 512 * 1024;
const FILE_LIMIT = 64 * 1024 * 1024;
const TOTAL_LIMIT = 256 * 1024 * 1024;

export function fileLimitMessage(files) {
  if (files.length > 32) return 'Choose at most 32 files for the preview.';
  if (files.some((file) => file.size > FILE_LIMIT)) return 'That file is too big for the preview (64 MiB at most).';
  if (files.reduce((sum, file) => sum + file.size, 0) > TOTAL_LIMIT) return 'Those files are too big for the preview (256 MiB total at most).';
  return null;
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
      await design.filePart({ session, transfer, file, name: entry.name, size: entry.size, offset, data: encode(bytes) });
      progress(entry.name, entry.size ? Math.round((offset + bytes.length) / entry.size * 100) : 100);
    }
  }
  return transfer;
}
