import { useCallback, useSyncExternalStore } from 'react';
import { applyTheme, readStoredTheme, THEME_STORAGE_KEY, type ThemePreference } from './theme';

const listeners = new Set<() => void>();

// Set only once a write to storage actually fails, so a selection still
// sticks for the rest of this tab's session even though it was never
// persisted. `readStoredTheme` always reports "system" once reads fail too,
// so without this the control could show System while the page stayed dark.
// Cleared on the next successful write, so working storage remains the
// source of truth once it recovers. Resets on reload, same as storage would.
let unpersistedPreference: ThemePreference | null = null;

function emit() {
  for (const listener of listeners) listener();
}

function subscribe(onChange: () => void): () => void {
  listeners.add(onChange);
  // Keep other tabs in step.
  window.addEventListener('storage', onChange);
  return () => {
    listeners.delete(onChange);
    window.removeEventListener('storage', onChange);
  };
}

function getSnapshot(): ThemePreference {
  return unpersistedPreference ?? readStoredTheme();
}

/**
 * The theme preference, read through useSyncExternalStore for the same reason
 * as the motion preference: it lives outside React (the DOM attribute and
 * localStorage), so it is subscribed to rather than mirrored into state.
 */
export function useTheme(): [ThemePreference, (next: ThemePreference) => void] {
  const preference = useSyncExternalStore(subscribe, getSnapshot, () => 'system' as const);

  const setPreference = useCallback((next: ThemePreference) => {
    try {
      localStorage.setItem(THEME_STORAGE_KEY, next);
      unpersistedPreference = null;
    } catch {
      // Storage is blocked: keep the selection in memory for this tab so
      // the control and the page agree until the next reload.
      unpersistedPreference = next;
    }
    applyTheme(next);
    emit();
  }, []);

  return [preference, setPreference];
}
