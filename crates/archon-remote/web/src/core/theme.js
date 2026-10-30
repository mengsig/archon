// Theme preference (system, light or dark), resolved onto <html data-theme>.
const KEY = 'archon.theme';
export const THEMES = ['system', 'light', 'dark'];
const THEME_COLORS = { light: '#f5f5f7', dark: '#000000' };

export function themePreference() {
  const stored = readStored();
  return THEMES.includes(stored) ? stored : 'system';
}

function readStored() {
  try {
    return localStorage.getItem(KEY);
  } catch {
    return null; // Storage can be blocked (private modes); the system theme applies.
  }
}

export function setThemePreference(theme) {
  if (!THEMES.includes(theme)) throw new RangeError(`unknown theme ${theme}`);
  localStorage.setItem(KEY, theme);
  applyTheme();
}

export function applyTheme() {
  const pref = themePreference();
  const dark = pref === 'dark' || (pref === 'system' && matchMedia('(prefers-color-scheme: dark)').matches);
  const resolved = dark ? 'dark' : 'light';
  document.documentElement.dataset.theme = resolved;
  document.querySelector('meta[name="theme-color"]')?.setAttribute('content', THEME_COLORS[resolved]);
  return resolved;
}

export function followSystemTheme() {
  matchMedia('(prefers-color-scheme: dark)').addEventListener('change', applyTheme);
  applyTheme();
}
