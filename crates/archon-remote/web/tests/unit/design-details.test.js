import test from 'node:test';
import assert from 'node:assert/strict';
import { buildDetails, noteFields, capText } from '../../src/features/design/details.js';

const target = {
  path: '/settings', selector: 'form#settings > button.primary', role_name: 'button "Save changes"', component: { name: 'SaveButton', framework: 'react' },
  box: { x: 12.4, y: 40, w: 120, h: 36 }, region: false, styles: { color: 'rgb(0,0,0)', display: 'flex', 'border-radius': '8px' },
};

test('the details block lists path, element, selector, component, box and styles', () => {
  const text = buildDetails(target);
  assert.deepEqual(text.split('\n'), [
    'Page: /settings', 'Element: button "Save changes"', 'Selector: form#settings > button.primary', 'Component: SaveButton (react)',
    'Box: x 12, y 40, 120 × 36', 'Styles: color: rgb(0,0,0); border-radius: 8px; display: flex',
  ]);
  assert.match(buildDetails({ ...target, region: true }), /Region the operator drew/);
  assert.equal(buildDetails(null), '');
});

test('the details cap at 2000 characters', () => {
  assert.equal(buildDetails({ ...target, selector: 'a'.repeat(5000) }).length, 2000);
  assert.equal(capText('x'.repeat(10), 3), 'xxx');
});

test('the note fields send exactly what the switches allow', () => {
  const base = { session: 'ds_1', clientKey: 'web-1', text: ' make it green ', asset: 'da_1', sendPicture: true, sendDetails: true, details: 'Page: /x' };
  assert.deepEqual(noteFields(base), { session: 'ds_1', client_key: 'web-1', text: 'make it green', asset: 'da_1', send_picture: true, details_text: 'Page: /x' });
  const off = noteFields({ ...base, sendPicture: false, sendDetails: false });
  assert.equal(off.send_picture, false);
  assert.equal(off.details_text, null);
  assert.equal(noteFields({ ...base, asset: null }).send_picture, false, 'no picture, nothing to send');
});

test('a follow-up and its references go into the note fields only when used', () => {
  const base = { session: 'ds_1', clientKey: 'web-1', text: 'darker', asset: null, sendPicture: false, sendDetails: false, details: '' };
  assert.deepEqual(Object.keys(noteFields(base)).sort(), ['asset', 'client_key', 'details_text', 'send_picture', 'session', 'text']);
  assert.deepEqual(noteFields({ ...base, follows: 'D3', refs: ['D5', 'D7'] }), { session: 'ds_1', client_key: 'web-1', text: 'darker', asset: null, send_picture: false, details_text: null, refs: ['D5', 'D7'], follows: 'D3' });
});
