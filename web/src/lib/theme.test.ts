import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import {
  applyTheme,
  isThemePreference,
  readStoredTheme,
  THEME_STORAGE_KEY,
  updateThemeColor,
} from './theme';

describe('theme preference', () => {
  beforeEach(() => {
    localStorage.clear();
    document.documentElement.removeAttribute('data-theme');
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it('defaults to following the system', () => {
    expect(readStoredTheme()).toBe('system');
  });

  it('reports a refusal to read rather than following the system', () => {
    // Private browsing or a blocked partition: the caller keeps its own
    // selection, so this must not look like "nothing saved".
    vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
      throw new DOMException('storage is blocked', 'SecurityError');
    });
    expect(readStoredTheme()).toBeNull();
  });

  it('round-trips an explicit choice', () => {
    localStorage.setItem(THEME_STORAGE_KEY, 'dark');
    expect(readStoredTheme()).toBe('dark');
  });

  it('ignores a corrupted stored value', () => {
    localStorage.setItem(THEME_STORAGE_KEY, 'neon');
    expect(readStoredTheme()).toBe('system');
  });

  it('validates preferences', () => {
    expect(isThemePreference('light')).toBe(true);
    expect(isThemePreference('dark')).toBe(true);
    expect(isThemePreference('system')).toBe(true);
    expect(isThemePreference('sepia')).toBe(false);
    expect(isThemePreference(null)).toBe(false);
  });

  it('removes the attribute for "system" so the media query stays authoritative', () => {
    applyTheme('dark');
    expect(document.documentElement.getAttribute('data-theme')).toBe('dark');

    applyTheme('system');
    expect(document.documentElement.hasAttribute('data-theme')).toBe(false);
  });

  it('applies explicit themes', () => {
    applyTheme('light');
    expect(document.documentElement.getAttribute('data-theme')).toBe('light');
  });
});

describe('theme-color meta', () => {
  let meta: HTMLMetaElement;

  beforeEach(() => {
    meta = document.createElement('meta');
    meta.name = 'theme-color';
    meta.content = '#090b0a';
    document.head.appendChild(meta);
  });

  afterEach(() => {
    document.head.removeChild(meta);
    vi.restoreAllMocks();
  });

  it('sets the light canvas color for light', () => {
    updateThemeColor('light');
    expect(meta.content).toBe('#f2f0f1');
  });

  it('sets the dark canvas color for dark', () => {
    updateThemeColor('dark');
    expect(meta.content).toBe('#0e0f11');
  });

  it('resolves system mode via matchMedia — light OS', () => {
    vi.spyOn(window, 'matchMedia').mockReturnValue({
      matches: false,
      media: '(prefers-color-scheme: dark)',
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    } as unknown as MediaQueryList);
    updateThemeColor('system');
    expect(meta.content).toBe('#f2f0f1');
  });

  it('resolves system mode via matchMedia — dark OS', () => {
    vi.spyOn(window, 'matchMedia').mockReturnValue({
      matches: true,
      media: '(prefers-color-scheme: dark)',
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    } as unknown as MediaQueryList);
    updateThemeColor('system');
    expect(meta.content).toBe('#0e0f11');
  });

  it('does nothing when the meta tag is absent', () => {
    document.head.removeChild(meta);
    expect(() => updateThemeColor('dark')).not.toThrow();
    document.head.appendChild(meta);
  });

  it('applyTheme updates the meta tag alongside the data-theme attribute', () => {
    applyTheme('dark');
    expect(document.documentElement.getAttribute('data-theme')).toBe('dark');
    expect(meta.content).toBe('#0e0f11');

    applyTheme('light');
    expect(document.documentElement.getAttribute('data-theme')).toBe('light');
    expect(meta.content).toBe('#f2f0f1');
  });
});
