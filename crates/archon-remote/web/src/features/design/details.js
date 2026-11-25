// The details block of a comment: what the composer shows and sends, as plain
// editable text. The operator can change anything in it before sending.
import { MAX_DETAILS_CHARS } from '../../core/design.js';

const SHOWN_STYLES = ['color', 'background-color', 'font-size', 'font-weight', 'padding', 'margin', 'border-radius', 'gap', 'display'];

export function buildDetails(target) {
  if (!target) return '';
  const lines = [];
  if (target.path) lines.push(`Page: ${target.path}`);
  if (target.region) lines.push('Region the operator drew');
  if (target.role_name) lines.push(`Element: ${target.role_name}`);
  if (target.selector) lines.push(`Selector: ${target.selector}`);
  if (target.component?.name) lines.push(`Component: ${target.component.name}${target.component.framework ? ` (${target.component.framework})` : ''}`);
  const box = target.box;
  if (box) lines.push(`Box: x ${Math.round(box.x)}, y ${Math.round(box.y)}, ${Math.round(box.w)} × ${Math.round(box.h)}`);
  const styles = target.styles ?? {};
  const keys = [...SHOWN_STYLES.filter((k) => k in styles), ...Object.keys(styles).filter((k) => !SHOWN_STYLES.includes(k))].slice(0, 12);
  if (keys.length) lines.push(`Styles: ${keys.map((k) => `${k}: ${styles[k]}`).join('; ')}`);
  return capText(lines.join('\n'), MAX_DETAILS_CHARS);
}

export function capText(text, max) {
  const chars = [...String(text ?? '')];
  return chars.length > max ? chars.slice(0, max).join('') : chars.join('');
}

// The fields of design_note_add for what the composer shows.
export function noteFields({ session, clientKey, text, asset, sendPicture, sendDetails, details, refs = [], follows = null }) {
  return {
    session,
    client_key: clientKey,
    text: capText(text.trim(), 4000),
    asset: asset ?? null,
    send_picture: Boolean(asset) && sendPicture,
    details_text: sendDetails && details.trim() ? capText(details.trim(), MAX_DETAILS_CHARS) : null,
    // Only sent when used, so a daemon without design iterations sees the note it always did.
    ...(refs.length ? { refs } : {}),
    ...(follows ? { follows } : {}),
  };
}
