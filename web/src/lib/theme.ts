export type ThemePreference = 'light' | 'dark' | 'system';

export const THEME_STORAGE_KEY = 'ths-theme';

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
