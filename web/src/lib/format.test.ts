import { describe, expect, it } from 'vitest';
import { absoluteTime, parseApiTimestamp, shortHash, timeAgo } from './format';

describe('parseApiTimestamp', () => {
  it("treats the API's naive SQLite timestamps as UTC", () => {
    // Regression: `new Date('2026-09-17 10:31:48')` parses as LOCAL time, which
    // shifted every activity row by the viewer's UTC offset.
    expect(parseApiTimestamp('2026-09-17 10:31:48')?.toISOString()).toBe(
      '2026-09-17T10:31:48.000Z',
    );
  });

  it('respects an explicit zone when one is present', () => {
    expect(parseApiTimestamp('2026-09-17T10:31:48Z')?.toISOString()).toBe(
      '2026-09-17T10:31:48.000Z',
    );
  });

  it('returns null for unusable input', () => {
    expect(parseApiTimestamp('')).toBeNull();
    expect(parseApiTimestamp('not-a-date')).toBeNull();
  });
});

describe('timeAgo', () => {
  const now = new Date('2026-09-17T12:00:00Z');

  it('describes recent and older events', () => {
    expect(timeAgo('2026-09-17 11:59:58', now)).toBe('just now');
    expect(timeAgo('2026-09-17 11:56:00', now)).toBe('4 minutes ago');
    expect(timeAgo('2026-09-16 12:00:00', now)).toBe('yesterday');
  });

  it('falls back safely', () => {
    expect(timeAgo('nonsense', now)).toBe('—');
  });

  it('switches units before displaying 60 seconds or 60 minutes', () => {
    expect(timeAgo('2026-09-17T11:59:00.400Z', now)).toBe('1 minute ago');
    expect(timeAgo('2026-09-17 11:00:20', now)).toBe('1 hour ago');
  });
});

describe('shortHash', () => {
  it('truncates long hashes and passes short values through', () => {
    expect(shortHash('cb45d3bb523989b81da2b91ba7c5cdfeeace2f6e1a34a39738905c')).toBe(
      'cb45d3bb5…38905c',
    );
    expect(shortHash('abc')).toBe('abc');
    expect(shortHash('')).toBe('—');
  });
});

describe('absoluteTime', () => {
  it('renders a locale string or an em dash', () => {
    expect(absoluteTime('2026-09-17 10:31:48')).not.toBe('—');
    expect(absoluteTime('')).toBe('—');
  });
});
