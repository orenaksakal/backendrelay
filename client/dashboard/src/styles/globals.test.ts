import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';

/**
 * WCAG 2.2 AA check for the B.1 design tokens.
 *
 * The tokens live in CSS, so the test reads `globals.css`, parses the `:root`
 * and `.dark` blocks and computes contrast ratios from the OKLCH values.
 * Text must reach 4.5:1 on every surface; control borders and focus rings 3:1.
 */
const source = readFileSync('src/styles/globals.css', 'utf8');
// Comments may sit next to declarations; drop them before parsing.
const css = source.replace(/\/\*[\s\S]*?\*\//g, '');

interface LinearRgb {
  r: number;
  g: number;
  b: number;
}

function readTokens(styles: string, selector: string): Map<string, string> {
  const start = styles.indexOf(`${selector} {`);
  if (start === -1) throw new Error(`globals.css has no ${selector} block`);

  const open = styles.indexOf('{', start);
  const close = styles.indexOf('}', open);
  if (open === -1 || close === -1) {
    throw new Error(`globals.css has an unclosed ${selector} block`);
  }

  const tokens = new Map<string, string>();
  for (const declaration of styles.slice(open + 1, close).split(';')) {
    const colon = declaration.indexOf(':');
    if (colon === -1) continue;

    const name = declaration.slice(0, colon).trim();
    const value = declaration.slice(colon + 1).trim();
    if (name.startsWith('--')) tokens.set(name, value);
  }
  return tokens;
}

function clamp(value: number): number {
  return Math.min(1, Math.max(0, value));
}

/** OKLCH to linear-light sRGB (the space WCAG luminance is defined in). */
function oklchToLinearRgb(
  lightness: number,
  chroma: number,
  hue: number,
): LinearRgb {
  const radians = (hue * Math.PI) / 180;
  const a = chroma * Math.cos(radians);
  const b = chroma * Math.sin(radians);

  const l = (lightness + 0.3963377774 * a + 0.2158037573 * b) ** 3;
  const m = (lightness - 0.1055613458 * a - 0.0638541728 * b) ** 3;
  const s = (lightness - 0.0894841775 * a - 1.291485548 * b) ** 3;

  return {
    r: clamp(4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s),
    g: clamp(-1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s),
    b: clamp(-0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s),
  };
}

function colorOf(tokens: Map<string, string>, name: string): LinearRgb {
  const value = tokens.get(name);
  if (!value) throw new Error(`globals.css has no ${name} token`);

  const match = /^oklch\(\s*([0-9.]+)\s+([0-9.]+)\s+([0-9.]+)\s*\)$/.exec(
    value,
  );
  if (!match) {
    throw new Error(`Unsupported colour for ${name}: ${value}`);
  }
  const lightness = Number(match[1]);
  const chroma = Number(match[2]);
  const hue = Number(match[3]);
  return oklchToLinearRgb(lightness, chroma, hue);
}

function luminance({ r, g, b }: LinearRgb): number {
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

function contrastRatio(
  tokens: Map<string, string>,
  foreground: string,
  background: string,
): number {
  const first = luminance(colorOf(tokens, foreground));
  const second = luminance(colorOf(tokens, background));
  const lighter = Math.max(first, second);
  const darker = Math.min(first, second);
  return (lighter + 0.05) / (darker + 0.05);
}

/** CSS alpha compositing happens in sRGB, before WCAG luminance. */
function composite(
  foreground: LinearRgb,
  background: LinearRgb,
  alpha: number,
): LinearRgb {
  const mix = (front: number, back: number): number =>
    alpha * toSrgb(front) + (1 - alpha) * toSrgb(back);
  return {
    r: fromSrgb(mix(foreground.r, background.r)),
    g: fromSrgb(mix(foreground.g, background.g)),
    b: fromSrgb(mix(foreground.b, background.b)),
  };
}

function toSrgb(channel: number): number {
  return channel <= 0.0031308
    ? 12.92 * channel
    : 1.055 * channel ** (1 / 2.4) - 0.055;
}

function fromSrgb(channel: number): number {
  return channel <= 0.04045
    ? channel / 12.92
    : ((channel + 0.055) / 1.055) ** 2.4;
}

/** Tone badges draw the token as text on that token at 12% over a surface. */
function toneContrast(
  tokens: Map<string, string>,
  tone: string,
  surface: string,
): number {
  const foreground = colorOf(tokens, tone);
  const background = composite(foreground, colorOf(tokens, surface), 0.12);
  const first = luminance(foreground);
  const second = luminance(background);
  return (Math.max(first, second) + 0.05) / (Math.min(first, second) + 0.05);
}

const THEMES = [
  { name: 'light', selector: ':root' },
  { name: 'dark', selector: '.dark' },
] as const;

const SURFACES = ['--background', '--card', '--sidebar', '--muted'] as const;

// Badge tones draw the token as text on the same token at 12% (badge.tsx).
const TONE_TOKENS = [
  '--accent',
  '--success',
  '--warning',
  '--destructive',
] as const;

// Foreground text, secondary text and links.
const TEXT_TOKENS = ['--foreground', '--muted-foreground', '--accent'] as const;

// Controls sit on the canvas, cards and the top bar, never on hover/code rows.
const CONTROL_SURFACES = ['--background', '--card', '--sidebar'] as const;

function belowThreshold(
  tokens: Map<string, string>,
  foregrounds: readonly string[],
  backgrounds: readonly string[],
  threshold: number,
): string[] {
  const failures: string[] = [];
  for (const foreground of foregrounds) {
    for (const background of backgrounds) {
      const ratio = contrastRatio(tokens, foreground, background);
      if (ratio < threshold) {
        failures.push(
          `${foreground} on ${background}: ${ratio.toFixed(2)}:1 (needs ${String(threshold)}:1)`,
        );
      }
    }
  }
  return failures;
}

describe.each(THEMES)('$name theme contrast', ({ selector }) => {
  const tokens = readTokens(css, selector);

  it('keeps text at 4.5:1 on every surface', () => {
    expect(belowThreshold(tokens, TEXT_TOKENS, SURFACES, 4.5)).toEqual([]);
  });

  it('keeps primary button text at 4.5:1', () => {
    expect(
      contrastRatio(tokens, '--primary-foreground', '--primary'),
    ).toBeGreaterThanOrEqual(4.5);
  });

  it('keeps control borders and focus rings at 3:1', () => {
    expect(
      belowThreshold(tokens, ['--input', '--ring'], CONTROL_SURFACES, 3),
    ).toEqual([]);
  });

  it('keeps status badge text at 4.5:1', () => {
    const failures = TONE_TOKENS.flatMap((tone) =>
      SURFACES.map((surface) => ({
        tone,
        surface,
        ratio: toneContrast(tokens, tone, surface),
      })).filter(({ ratio }) => ratio < 4.5),
    );

    expect(
      failures.map(
        ({ tone, surface, ratio }) =>
          `${tone} on ${surface}: ${ratio.toFixed(2)}:1 (needs 4.5:1)`,
      ),
    ).toEqual([]);
  });
});
