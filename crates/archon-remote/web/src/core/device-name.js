// A default device name from the user agent; the user can always edit it.
const RULES = [
  [/iPad/, 'iPad'],
  [/iPhone|iPod/, 'iPhone'],
  [/Android.*Mobile/, 'Android phone'],
  [/Android/, 'Android tablet'],
  [/Macintosh|Mac OS X/, 'Mac'],
  [/Windows/, 'Windows PC'],
  [/CrOS/, 'Chromebook'],
  [/Linux/, 'Linux computer'],
];
export const MAX_DEVICE_NAME = 64;

export function suggestDeviceName(userAgent) {
  const ua = String(userAgent ?? '');
  const match = RULES.find(([pattern]) => pattern.test(ua));
  return match ? match[1] : 'Browser';
}

export function deviceNameProblem(name) {
  const trimmed = String(name ?? '').trim();
  if (trimmed.length === 0) return 'Give this device a name.';
  if ([...trimmed].length > MAX_DEVICE_NAME) return `Keep the name to ${MAX_DEVICE_NAME} characters.`;
  return null;
}
