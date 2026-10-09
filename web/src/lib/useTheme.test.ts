import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, renderHook } from '@testing-library/react';
import { THEME_STORAGE_KEY } from './theme';

/**
 * The store keeps the selection it could not store in module state, so every
 * test mounts a fresh copy of it.
 */
async function mountTheme() {
  vi.resetModules();
  const { useTheme } = await import('./useTheme');
  return renderHook(() => useTheme());
}

/** A storage that refuses, as private browsing and a blocked partition do. */
function refuseStorage(method: 'getItem' | 'setItem') {
  vi.spyOn(Storage.prototype, method).mockImplementation(() => {
    throw new DOMException('storage is blocked', 'SecurityError');
  });
}

describe('useTheme', () => {
  beforeEach(() => {
    localStorage.clear();
    document.documentElement.removeAttribute('data-theme');
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it('reports the stored preference and persists a selection', async () => {
    const { result } = await mountTheme();
    expect(result.current[0]).toBe('system');

    act(() => result.current[1]('dark'));

    expect(result.current[0]).toBe('dark');
    expect(document.documentElement.getAttribute('data-theme')).toBe('dark');
    expect(localStorage.getItem(THEME_STORAGE_KEY)).toBe('dark');
  });

  it('follows a preference another tab stored, and its removal', async () => {
    const { result } = await mountTheme();
    // A selection this tab made first, so the removal below has an in-memory
    // value to lose to: storage stays authoritative wherever it answers.
    act(() => result.current[1]('dark'));
    expect(result.current[0]).toBe('dark');

    act(() => {
      localStorage.setItem(THEME_STORAGE_KEY, 'light');
      window.dispatchEvent(
        new StorageEvent('storage', { key: THEME_STORAGE_KEY, newValue: 'light' }),
      );
    });
    expect(result.current[0]).toBe('light');

    act(() => {
      localStorage.removeItem(THEME_STORAGE_KEY);
      window.dispatchEvent(new StorageEvent('storage', { key: THEME_STORAGE_KEY, newValue: null }));
    });
    expect(result.current[0]).toBe('system');
  });

  it('keeps the selection when storage refuses reads and writes', async () => {
    refuseStorage('getItem');
    refuseStorage('setItem');
    const { result } = await mountTheme();
    expect(result.current[0]).toBe('system');

    act(() => result.current[1]('dark'));
    expect(result.current[0]).toBe('dark');
    expect(document.documentElement.getAttribute('data-theme')).toBe('dark');

    act(() => result.current[1]('light'));
    expect(result.current[0]).toBe('light');
    expect(document.documentElement.getAttribute('data-theme')).toBe('light');

    act(() => result.current[1]('system'));
    expect(result.current[0]).toBe('system');
    expect(document.documentElement.hasAttribute('data-theme')).toBe(false);
  });

  it('keeps the selection when storage refuses the read but takes the write', async () => {
    refuseStorage('getItem');
    const setItem = vi.spyOn(Storage.prototype, 'setItem');
    const { result } = await mountTheme();
    expect(result.current[0]).toBe('system');

    act(() => result.current[1]('dark'));

    expect(setItem).toHaveBeenCalledWith(THEME_STORAGE_KEY, 'dark');
    expect(result.current[0]).toBe('dark');
    expect(document.documentElement.getAttribute('data-theme')).toBe('dark');
  });

  it('follows storage again once a write lands', async () => {
    // The sequence the review asked for: a refused write, then one that lands,
    // then another tab's change. A refusal flag that never clears would ignore
    // the last step and the control would stay on this tab's own selection.
    vi.spyOn(Storage.prototype, 'setItem').mockImplementationOnce(() => {
      throw new DOMException('storage is blocked', 'SecurityError');
    });
    const { result } = await mountTheme();

    act(() => result.current[1]('dark'));
    expect(result.current[0]).toBe('dark');

    act(() => result.current[1]('light'));
    expect(result.current[0]).toBe('light');
    expect(localStorage.getItem(THEME_STORAGE_KEY)).toBe('light');

    act(() => {
      localStorage.removeItem(THEME_STORAGE_KEY);
      window.dispatchEvent(new StorageEvent('storage', { key: THEME_STORAGE_KEY, newValue: null }));
    });
    expect(result.current[0]).toBe('system');
  });

  it('follows storage again once a read succeeds', async () => {
    // The read side of the same property: a refusal must not outlive the read.
    const getItem = vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
      throw new DOMException('storage is blocked', 'SecurityError');
    });
    const { result } = await mountTheme();

    act(() => result.current[1]('dark'));
    expect(result.current[0]).toBe('dark');

    getItem.mockRestore();
    act(() => {
      localStorage.setItem(THEME_STORAGE_KEY, 'light');
      window.dispatchEvent(
        new StorageEvent('storage', { key: THEME_STORAGE_KEY, newValue: 'light' }),
      );
    });
    expect(result.current[0]).toBe('light');
  });

  it('keeps the selection when storage reads but refuses the write', async () => {
    refuseStorage('setItem');
    const { result } = await mountTheme();

    act(() => result.current[1]('dark'));

    expect(result.current[0]).toBe('dark');
    expect(document.documentElement.getAttribute('data-theme')).toBe('dark');
  });
});
