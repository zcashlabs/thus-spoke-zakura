export type ThemePreference = 'light' | 'dark' | 'system';

export const THEME_STORAGE_KEY = 'ths-theme';

// Canvas colors used for <meta name="theme-color">. Must stay in sync with
// --color-canvas in globals.css for each theme.
const THEME_CANVAS: Record<'light' | 'dark', string> = {
  light: '#f2f0f1',
  dark: '#0e0f11',
};

export function isThemePreference(value: unknown): value is ThemePreference {
  return value === 'light' || value === 'dark' || value === 'system';
}

/**
 * Applies a preference to the document.
 *
 * "system" removes the attribute entirely rather than resolving it here, so the
 * CSS media query stays the single source of truth and the theme keeps
 * following the OS if it changes while the tab is open.
 */
export function applyTheme(preference: ThemePreference): void {
  const root = document.documentElement;
  if (preference === 'system') root.removeAttribute('data-theme');
  else root.setAttribute('data-theme', preference);
  updateThemeColor(preference);
}

/**
 * Updates <meta name="theme-color"> to match the resolved canvas color.
 *
 * Called from applyTheme and from a matchMedia listener in useTheme so that
 * the mobile browser toolbar stays in step with every kind of theme change:
 * an explicit toggle, a cross-tab storage event, or an OS preference change
 * while the page is following the system.
 */
export function updateThemeColor(preference: ThemePreference): void {
  const meta = document.querySelector<HTMLMetaElement>('meta[name="theme-color"]');
  if (!meta) return;
  const dark =
    preference === 'dark' ||
    (preference === 'system' && window.matchMedia('(prefers-color-scheme: dark)').matches);
  meta.content = dark ? THEME_CANVAS.dark : THEME_CANVAS.light;
}

/**
 * Reads the stored preference.
 *
 * `null` means storage would not answer — private browsing, a blocked storage
 * partition. That is not the same as "nothing saved", which is `system`: the
 * caller keeps its own selection in the first case, and drops it in the second.
 */
export function readStoredTheme(): ThemePreference | null {
  try {
    const stored = localStorage.getItem(THEME_STORAGE_KEY);
    return isThemePreference(stored) ? stored : 'system';
  } catch {
    return null;
  }
}
