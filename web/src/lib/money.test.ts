import { describe, expect, it } from 'vitest';
import { formatZec, formatZecAmount, parseZec, ZATOSHIS_PER_ZEC } from './money';

describe('parseZec', () => {
  it('parses whole and fractional amounts exactly', () => {
    expect(parseZec('1')).toBe(ZATOSHIS_PER_ZEC);
    expect(parseZec('0.00000001')).toBe(1n);
    expect(parseZec('5')).toBe(500_000_000n);
    expect(parseZec('  2.5  ')).toBe(250_000_000n);
    expect(parseZec('.5')).toBe(50_000_000n);
    expect(parseZec('0')).toBe(0n);
  });

  it('is exact for values that are not representable as floats', () => {
    // Number('2.675') * 1e8 is 267499999.99999997 and Number('81.4') * 1e8 is
    // 8140000000.000001. Rounding happens to recover both, so these pin
    // exactness rather than proving the parser differs from the old one.
    expect(parseZec('2.675')).toBe(267_500_000n);
    expect(parseZec('81.4')).toBe(8_140_000_000n);
    expect(parseZec('0.07')).toBe(7_000_000n);
  });

  it('stays exact where rounding a float no longer recovers the value', () => {
    // Above ~1e8 ZEC the float error exceeds half a zatoshi and
    // Math.round(Number(x) * 1e8) returns the wrong integer. This is beyond
    // the 21M supply cap, so it is not reachable with real funds — it is here
    // because it is the only input that distinguishes an exact parser from
    // one that is accidentally correct.
    expect(parseZec('99999999.54960525')).toBe(9_999_999_954_960_525n);
    expect(Math.round(Number('99999999.54960525') * 1e8)).toBe(9_999_999_954_960_526);
  });

  it('stays exact far beyond Number.MAX_SAFE_INTEGER', () => {
    expect(parseZec('210000000')).toBe(21_000_000_000_000_000n);
  });

  it('rejects malformed input', () => {
    for (const bad of ['', ' ', '.', 'abc', '1.2.3', '-1', '1e8', '0.000000001', '١٢']) {
      expect(parseZec(bad)).toBeNull();
    }
  });
});

describe('formatZec', () => {
  it('trims insignificant zeros and groups thousands', () => {
    expect(formatZec(ZATOSHIS_PER_ZEC)).toBe('1');
    expect(formatZec(250_000_000n)).toBe('2.5');
    expect(formatZec(1n)).toBe('0.00000001');
    expect(formatZec(123_456_789_012_345n)).toBe('1,234,567.89012345');
    expect(formatZec(0n)).toBe('0');
  });

  it('keeps trailing zeros on request', () => {
    expect(formatZec(250_000_000n, { trailingZeros: true })).toBe('2.50000000');
  });

  it('leaves thousands ungrouped on request so the result parses back exactly', () => {
    for (const [value, text] of [
      [99_999_999_999n, '999.99999999'],
      [100_000_000_000n, '1000'],
      [100_000_000_001n, '1000.00000001'],
      [1n, '0.00000001'],
    ] as const) {
      expect(formatZec(value, { grouping: false })).toBe(text);
      expect(parseZec(text)).toBe(value);
    }
  });

  it('round-trips through parseZec', () => {
    for (const value of [0n, 1n, 54n, 500_000_000n, 267_500_000n, 8_140_000_000n]) {
      expect(parseZec(formatZec(value))).toBe(value);
    }
  });

  it('appends the unit for display', () => {
    expect(formatZecAmount(500_000_000n)).toBe('5 ZEC');
  });
});
