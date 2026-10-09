/**
 * ZEC amounts are handled as `bigint` zatoshi everywhere except display.
 *
 * The previous implementation used `Math.round(Number(input) * 1e8)`, which
 * routes money through a float: `Number('2.675') * 1e8` is
 * `267499999.99999997`. Rounding masked it at small magnitudes, but the
 * primitive was wrong. Parsing decimal strings digit-by-digit avoids the
 * float entirely.
 */

export const ZEC_DECIMALS = 8;
export const ZATOSHIS_PER_ZEC = 100_000_000n;

const AMOUNT_PATTERN = /^\d*(?:\.\d*)?$/;

/** Parses a plain-decimal ZEC string into zatoshi. Returns null if invalid. */
export function parseZec(input: string): bigint | null {
  const trimmed = input.trim();
  if (trimmed === '' || trimmed === '.' || !AMOUNT_PATTERN.test(trimmed)) return null;

  const [whole = '', fraction = ''] = trimmed.split('.');
  if (fraction.length > ZEC_DECIMALS) return null;

  const padded = fraction.padEnd(ZEC_DECIMALS, '0');
  return BigInt(whole || '0') * ZATOSHIS_PER_ZEC + BigInt(padded || '0');
}

/** Formats zatoshi as a ZEC string, trimming insignificant trailing zeros. */
export function formatZec(
  zatoshi: bigint,
  options?: { trailingZeros?: boolean; grouping?: boolean },
): string {
  const negative = zatoshi < 0n;
  const absolute = negative ? -zatoshi : zatoshi;

  const whole = absolute / ZATOSHIS_PER_ZEC;
  const fraction = (absolute % ZATOSHIS_PER_ZEC).toString().padStart(ZEC_DECIMALS, '0');
  const visible = options?.trailingZeros ? fraction : fraction.replace(/0+$/, '');

  const grouped =
    options?.grouping === false
      ? whole.toString()
      : whole.toString().replace(/\B(?=(\d{3})+(?!\d))/g, ',');
  return `${negative ? '-' : ''}${grouped}${visible ? `.${visible}` : ''}`;
}

/** Formats zatoshi for display, e.g. `1,234.5 ZEC`. */
export function formatZecAmount(zatoshi: bigint): string {
  return `${formatZec(zatoshi)} ZEC`;
}
