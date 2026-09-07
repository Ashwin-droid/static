import type { ThemePreset } from './themes';

export const THEME_FADE_MS = 3000;

export const themeKey = (theme: ThemePreset) => [theme.uiTheme.primary, theme.uiTheme.secondary,
  theme.uiTheme.background, theme.graphTheme.line1, theme.graphTheme.line2].join('/').toLowerCase();

/** Interpolate displayed RGB channels: yellow to blue passes through grey. */
export function mixColor(from: string, to: string, fraction: number): string {
  const a = Number.parseInt(from.slice(1), 16), b = Number.parseInt(to.slice(1), 16);
  const t = Math.max(0, Math.min(1, fraction));
  const channel = (shift: number) => Math.round(((a >> shift) & 255) * (1 - t) + ((b >> shift) & 255) * t);
  return `#${((channel(16) << 16) | (channel(8) << 8) | channel(0)).toString(16).padStart(6, '0')}`;
}

const copy = (theme: ThemePreset): ThemePreset => ({ name: theme.name,
  uiTheme: { ...theme.uiTheme }, graphTheme: { ...theme.graphTheme } });

/** A single wall-clock transition, sampled by the existing WebGL render loop. */
export class ThemeTransition {
  private from: ThemePreset;
  private target: ThemePreset;
  private started = Number.NEGATIVE_INFINITY;
  constructor(initial: ThemePreset) { this.from = copy(initial); this.target = copy(initial); }

  setTarget(next: ThemePreset, now: number) {
    if (themeKey(next) === themeKey(this.target)) return;
    this.from = this.sample(now);
    this.target = copy(next);
    this.started = now;
  }

  sample(now: number): ThemePreset {
    const fraction = Math.max(0, Math.min(1, (now - this.started) / THEME_FADE_MS));
    if (fraction === 1) return this.target;
    const mix = (a: string, b: string) => mixColor(a, b, fraction);
    return { name: this.target.name,
      uiTheme: {
        primary: mix(this.from.uiTheme.primary, this.target.uiTheme.primary),
        secondary: mix(this.from.uiTheme.secondary, this.target.uiTheme.secondary),
        background: mix(this.from.uiTheme.background, this.target.uiTheme.background),
      },
      graphTheme: {
        line1: mix(this.from.graphTheme.line1, this.target.graphTheme.line1),
        line2: mix(this.from.graphTheme.line2, this.target.graphTheme.line2),
      },
    };
  }
}
