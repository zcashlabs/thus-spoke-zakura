import { act, renderHook } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { THEME_STORAGE_KEY } from './theme';
import type { useTheme as UseTheme } from './useTheme';

// The in-memory fallback used when storage is blocked lives at module scope,
// the same way it would in a real page load. Reimporting with a reset module
// registry between tests mirrors a reload, instead of leaking a fallback
// value set in one test into the next.
let useTheme: typeof UseTheme;

beforeEach(async () => {
  vi.resetModules();
  localStorage.clear();
  document.documentElement.removeAttribute('data-theme');
  ({ useTheme } = await import('./useTheme'));
});

afterEach(() => {
  vi.restoreAllMocks();
});

function blockStorage() {
  vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
    throw new DOMException('blocked', 'SecurityError');
  });
  vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
    throw new DOMException('blocked', 'SecurityError');
  });
}

describe('useTheme', () => {
  it('keeps the page and the reported preference in sync while storage is blocked', () => {
    blockStorage();
    const { result } = renderHook(() => useTheme());

    act(() => result.current[1]('dark'));

    expect(result.current[0]).toBe('dark');
    expect(document.documentElement.getAttribute('data-theme')).toBe('dark');
  });

  it('keeps working across repeated selections while storage stays blocked', () => {
    blockStorage();
    const { result } = renderHook(() => useTheme());

    act(() => result.current[1]('dark'));
    expect(result.current[0]).toBe('dark');

    act(() => result.current[1]('light'));
    expect(result.current[0]).toBe('light');
    expect(document.documentElement.getAttribute('data-theme')).toBe('light');

    act(() => result.current[1]('system'));
    expect(result.current[0]).toBe('system');
    expect(document.documentElement.hasAttribute('data-theme')).toBe(false);
  });

  it('persists and reads back normally when storage works', () => {
    const { result } = renderHook(() => useTheme());

    act(() => result.current[1]('dark'));

    expect(result.current[0]).toBe('dark');
    expect(localStorage.getItem(THEME_STORAGE_KEY)).toBe('dark');

    const { result: other } = renderHook(() => useTheme());
    expect(other.current[0]).toBe('dark');
  });

  it('restores system behaviour once the stored preference is cleared', () => {
    const { result } = renderHook(() => useTheme());
    act(() => result.current[1]('dark'));
    expect(result.current[0]).toBe('dark');

    act(() => {
      localStorage.removeItem(THEME_STORAGE_KEY);
      window.dispatchEvent(new StorageEvent('storage', { key: THEME_STORAGE_KEY }));
    });

    expect(result.current[0]).toBe('system');
  });

  it('syncs a change made in another tab through the storage event', () => {
    const { result } = renderHook(() => useTheme());
    expect(result.current[0]).toBe('system');

    act(() => {
      localStorage.setItem(THEME_STORAGE_KEY, 'dark');
      window.dispatchEvent(new StorageEvent('storage', { key: THEME_STORAGE_KEY }));
    });

    expect(result.current[0]).toBe('dark');
  });
});
