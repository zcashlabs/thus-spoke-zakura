import { useCallback, useSyncExternalStore } from 'react';
import { applyTheme, readStoredTheme, THEME_STORAGE_KEY, type ThemePreference } from './theme';

const listeners = new Set<() => void>();

// The choice this tab made while storage would not take it, and whether the last
// write landed. A write that failed leaves the stored value stale, so this tab's
// own selection is what the snapshot reports until one succeeds — and a write
// that succeeds hands storage back, so another tab's change is followed again.
let selectedPreference: ThemePreference | null = null;
let writeUnsaved = false;

function readPreference(): ThemePreference {
  const stored = readStoredTheme();
  if (stored === null || writeUnsaved) {
    return selectedPreference ?? stored ?? 'system';
  }
  return stored;
}

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

/**
 * The theme preference, read through useSyncExternalStore for the same reason
 * as the motion preference: it lives outside React (the DOM attribute and
 * localStorage), so it is subscribed to rather than mirrored into state. When
 * storage refuses to answer — or holds a value this tab could not write — the
 * snapshot is the selection this tab made instead, so the control and the page
 * agree on it. Storage answers again the moment a read or a write succeeds.
 */
export function useTheme(): [ThemePreference, (next: ThemePreference) => void] {
  const preference = useSyncExternalStore(subscribe, readPreference, () => 'system' as const);

  const setPreference = useCallback((next: ThemePreference) => {
    selectedPreference = next;
    try {
      localStorage.setItem(THEME_STORAGE_KEY, next);
      // It landed: storage is the source of truth again, so the stored value and
      // another tab's changes are followed from here on.
      writeUnsaved = false;
    } catch {
      // Storage will not persist it: this tab keeps the choice until it can.
      writeUnsaved = true;
    }
    applyTheme(next);
    emit();
  }, []);

  return [preference, setPreference];
}
