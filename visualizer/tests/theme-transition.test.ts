import test from 'node:test';
import assert from 'node:assert/strict';
import { mixColor, ThemeTransition, themeKey, THEME_FADE_MS } from '../src/theme-transition';
import type { ThemePreset } from '../src/themes';

const palette = (color: string): ThemePreset => ({ name: 'Cover art',
  uiTheme: { primary: color, secondary: color, background: color },
  graphTheme: { line1: color, line2: color } });

test('the fade passes through every RGB channel value, without stages or hex-digit carry', () => {
  const fade = new ThemeTransition(palette('#ff0000'));
  fade.setTarget(palette('#00ff00'), 0);
  const colors = new Set<string>();
  for (let step = 0; step <= 255; step++) {
    const color = fade.sample(THEME_FADE_MS * step / 255).graphTheme.line1;
    colors.add(color);
    const value = Number.parseInt(color.slice(1), 16);
    assert.equal(value >> 16, 255 - step);
    assert.equal((value >> 8) & 255, step);
    assert.equal(value & 255, 0);
  }
  assert.equal(colors.size, 256);
  assert.equal(mixColor('#0f20ff', '#102000', 0.5), '#102080');
});

test('yellow to blue passes through RGB grey and completes at exactly three seconds', () => {
  const yellow = palette('#ffff00'), blue = palette('#0000ff');
  const fade = new ThemeTransition(yellow);
  fade.setTarget(blue, 100);
  assert.equal(themeKey(fade.sample(100)), themeKey(yellow));
  assert.equal(fade.sample(850).graphTheme.line1, '#bfbf40');
  assert.equal(themeKey(fade.sample(1600)), themeKey(palette('#808080')));
  assert.equal(fade.sample(2350).graphTheme.line1, '#4040bf');
  assert.equal(themeKey(fade.sample(100 + THEME_FADE_MS)), themeKey(blue));
  assert.equal(themeKey(fade.sample(10_000)), themeKey(blue));
});

test('rapid palette changes start from the live blend without a jump', () => {
  const fade = new ThemeTransition(palette('#ffff00'));
  fade.setTarget(palette('#0000ff'), 0);
  const middle = fade.sample(1500);
  fade.setTarget(palette('#ff0000'), 1500);
  assert.equal(themeKey(fade.sample(1500)), themeKey(middle));
  assert.equal(fade.sample(3000).uiTheme.background, '#c04040');
  assert.equal(fade.sample(4500).graphTheme.line2, '#ff0000');
});

test('repeated identical targets do not restart the transition', () => {
  const fade = new ThemeTransition(palette('#000000'));
  for (let now = 0; now <= 3000; now += 50) {
    fade.setTarget(palette(now % 100 ? '#FFFFFF' : '#ffffff'), now);
  }
  assert.equal(themeKey(fade.sample(3000)), themeKey(palette('#ffffff')));
});

test('all five palette colours share one timeline and skipped frames still finish on time', () => {
  const from: ThemePreset = { name: 'old', uiTheme: { primary: '#000000', secondary: '#204060', background: '#102030' }, graphTheme: { line1: '#ffff00', line2: '#ff0000' } };
  const to: ThemePreset = { name: 'new', uiTheme: { primary: '#ffffff', secondary: '#604020', background: '#302010' }, graphTheme: { line1: '#0000ff', line2: '#00ff00' } };
  const fade = new ThemeTransition(from);
  fade.setTarget(to, 500);
  assert.deepEqual(fade.sample(2000), { name: 'new', uiTheme: { primary: '#808080', secondary: '#404040', background: '#202020' }, graphTheme: { line1: '#808080', line2: '#808000' } });
  assert.equal(themeKey(fade.sample(9000)), themeKey(to));
  assert.equal(mixColor('#000000', '#ffffff', -1), '#000000');
  assert.equal(mixColor('#000000', '#ffffff', 2), '#ffffff');
});
