// Settings: theme, this device's pairing, forgetting it, and versions.
import { h } from '../../ui/dom.js';
import { screenHeader, section, card, row, segmented, describeError } from '../../ui/components.js';
import { confirmSheet } from '../../ui/sheet.js';
import { toast } from '../../ui/toast.js';
import { themePreference, setThemePreference } from '../../core/theme.js';
import { dateTime } from '../../core/format.js';
import * as api from '../../core/api.js';
import { APP_VERSION } from '../../version.js';

export function mountSettings(view, ctx) {
  const session = ctx.store.get().session;
  const back = { label: 'Back to overview', text: 'Overview', onClick: () => ctx.router.back('#/overview') };
  const forget = h('button', { class: 'button button-block button-danger-plain', type: 'button', onclick: () => forgetDevice(ctx, forget) }, 'Forget this device');
  view.append(
    screenHeader({ title: 'Settings', back }),
    section('Appearance', card(h('div', { class: 'row row-stack' },
      h('span', { class: 'row-label', text: 'Theme' }),
      segmented({
        label: 'Theme', value: themePreference(),
        options: [{ value: 'system', label: 'System' }, { value: 'light', label: 'Light' }, { value: 'dark', label: 'Dark' }],
        onChange: (theme) => setThemePreference(theme),
      })))),
    section('This device', card(
      row('Name', session.device.name),
      row('Paired', dateTime(session.device.paired_at)),
      row('Device id', session.device.id)),
    h('p', { class: 'section-foot', text: 'Forgetting unpairs this device on the host. You need a new code to pair it again.' }),
    forget),
    section('About', card(
      row('App', APP_VERSION),
      row('Archon on the host', session.daemon_version ?? '—'),
      row('Protocol', String(session.protocol_version)))));
  return () => {};
}

async function forgetDevice(ctx, button) {
  const sure = await confirmSheet({
    title: 'Forget this device?',
    message: 'It stops reaching Archon at once. Pairing again needs a fresh code from the host.',
    confirmLabel: 'Forget device',
  });
  if (!sure) return;
  button.disabled = true;
  try {
    await api.forget();
    ctx.onForget();
  } catch (err) {
    toast(describeError(err), { tone: 'red' });
    button.disabled = false;
  }
}
