import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';

/**
 * Guards the colour system against the failure that is invisible to every
 * other check: a token that type-checks, lints and builds, but renders as an
 * imperceptible step against the surface it sits on.
 *
 * Three effects shipped broken before this existed — the skeleton shimmer and
 * the balance flash were invisible in *both* themes, and the petals were
 * invisible in dark.
 */
// jsdom rewrites import.meta.url to an http URL, so resolve from the cwd.
const css = readFileSync(join(process.cwd(), 'src/styles/globals.css'), 'utf8');

function directiveBlock(header: string): string {
  const start = css.indexOf(header);
  if (start < 0) throw new Error(`${header} should exist`);
  const openingBrace = css.indexOf('{', start);
  let depth = 0;
  for (let i = openingBrace; i < css.length; i += 1) {
    if (css[i] === '{') depth += 1;
    if (css[i] === '}') depth -= 1;
    if (depth === 0) return css.slice(openingBrace + 1, i);
  }
  throw new Error(`${header} should have a closing brace`);
}

function tokens(scope: 'light' | 'dark'): Record<string, string> {
  const block = directiveBlock(scope === 'light' ? '@theme {' : '@utility theme-dark {');
  const found: Record<string, string> = {};
  for (const match of block.matchAll(/--color-([a-z0-9-]+):\s*(#[0-9a-fA-F]{6})/g)) {
    const name = match[1];
    const value = match[2];
    if (name && value) found[name] ??= value;
  }
  return found;
}

function relativeLuminance(hex: string): number {
  const channels = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255);
  const [r = 0, g = 0, b = 0] = channels.map((c) =>
    c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4,
  );
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

function contrast(a: string, b: string): number {
  const [hi, lo] = [relativeLuminance(a), relativeLuminance(b)].sort((x, y) => y - x) as [
    number,
    number,
  ];
  return (hi + 0.05) / (lo + 0.05);
}

/** A decorative element only registers above roughly this luminance step. */
const DECORATION_MIN = 1.25;
/** Text must clear WCAG AA for normal-size copy. */
const TEXT_MIN = 4.5;

describe.each(['light', 'dark'] as const)('%s theme', (scope) => {
  const t = tokens(scope);

  it('defines every token the other theme defines', () => {
    const other = tokens(scope === 'light' ? 'dark' : 'light');
    expect(Object.keys(t).sort()).toEqual(Object.keys(other).sort());
  });

  it.each([
    ['ink', 'panel'],
    ['ink', 'canvas'],
    ['ink-muted', 'panel'],
    ['ink-muted', 'canvas'],
    ['accent', 'panel'],
    ['accent-strong', 'panel'],
    ['accent-strong', 'accent-soft'],
    ['positive', 'positive-soft'],
    ['warning', 'warning-soft'],
    ['negative', 'negative-soft'],
  ])('%s is readable on %s', (fg, bg) => {
    expect(contrast(t[fg]!, t[bg]!)).toBeGreaterThanOrEqual(TEXT_MIN);
  });

  it.each([
    ['petal', 'panel'],
    ['petal-edge', 'panel'],
    ['shimmer', 'raised'],
    ['flash', 'panel'],
  ])('%s is perceptible against %s', (decoration, surface) => {
    expect(contrast(t[decoration]!, t[surface]!)).toBeGreaterThanOrEqual(DECORATION_MIN);
  });
});

/**
 * The bevel must feel like the same physical control in both themes. Which
 * edge carries the relief flips with the substrate, so the assertion is on
 * relief *strength*, not on the highlight:shadow ratio.
 */
describe('bevel relief', () => {
  const FACE = { light: 'raised', dark: 'raised' } as const;

  it.each(['light', 'dark'] as const)('%s has a perceptible raised edge', (scope) => {
    const t = tokens(scope);
    const face = t[FACE[scope]]!;
    const relief = Math.max(contrast(t['bevel-light']!, face), contrast(t['bevel-dark']!, face));
    expect(relief).toBeGreaterThanOrEqual(1.7);
  });

  it('gives both themes comparable relief', () => {
    const strength = (scope: 'light' | 'dark') => {
      const t = tokens(scope);
      const face = t['raised']!;
      return Math.max(contrast(t['bevel-light']!, face), contrast(t['bevel-dark']!, face));
    };
    const [light, dark] = [strength('light'), strength('dark')];
    expect(Math.max(light, dark) / Math.min(light, dark)).toBeLessThan(1.35);
  });
});
