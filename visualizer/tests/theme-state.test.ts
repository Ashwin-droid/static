import { test } from 'node:test';
import assert from 'node:assert/strict';
import { activeTheme, initialThemeSelection, selectTheme } from '../src/theme-state.ts';
import { presets, type ThemePreset } from '../src/themes.ts';

const cover: ThemePreset = { name: 'Cover art', uiTheme: { primary: '#ffddee', secondary: '#BBccAA', background: '#170b10' },
  graphTheme: { line1: '#abcdef', line2: '#edcbaa' } };
const startSong = () => selectTheme(initialThemeSelection(), { type: 'track', videoId: 'song-a' });

test('cover mode starts with fallback and uses only the current song palette', () => {
  let state = startSong();
  assert.equal(state.mode, 'cover');
  assert.equal(activeTheme(state), presets[0]);
  assert.equal(selectTheme(state, { type: 'cover', videoId: 'old-song', theme: cover }), state);
  state = selectTheme(state, { type: 'cover', videoId: 'song-a', theme: cover });
  assert.deepEqual(activeTheme(state), cover);
  assert.equal(selectTheme(state, { type: 'track', videoId: 'song-a' }), state);
  state = selectTheme(state, { type: 'track', videoId: 'song-b' });
  assert.equal(state.cover, null);
  assert.equal(activeTheme(state), presets[0]);
  assert.equal(selectTheme(state, { type: 'cover', videoId: 'song-a', theme: cover }), state);
});

test('manual presets survive track changes and palettes arriving after selection', () => {
  let state = selectTheme(startSong(), { type: 'manual', theme: presets[5] });
  state = selectTheme(state, { type: 'cover', videoId: 'song-a', theme: cover });
  assert.equal(activeTheme(state), presets[5]);
  state = selectTheme(state, { type: 'track', videoId: 'song-b' });
  assert.equal(activeTheme(state), presets[5]);
  const nextCover = { ...cover, graphTheme: { line1: '#112233', line2: '#445566' } };
  state = selectTheme(state, { type: 'cover', videoId: 'song-b', theme: nextCover });
  assert.equal(activeTheme(state), presets[5]);
  state = selectTheme(state, { type: 'automatic' });
  assert.deepEqual(activeTheme(state), nextCover);
});

test('custom colours survive artwork refresh; returning to cover mode applies its latest palette', () => {
  let state = selectTheme(startSong(), { type: 'cover', videoId: 'song-a', theme: cover });
  const custom = { ...activeTheme(state), name: '', graphTheme: { ...cover.graphTheme, line1: '#010203' } };
  state = selectTheme(state, { type: 'manual', theme: custom });
  state = selectTheme(state, { type: 'cover', videoId: 'song-a', theme: null });
  assert.equal(activeTheme(state), custom);
  assert.equal(state.cover, null);
  state = selectTheme(state, { type: 'automatic' });
  assert.equal(activeTheme(state), presets[0]);
  state = selectTheme(state, { type: 'cover', videoId: 'song-a', theme: cover });
  assert.deepEqual(activeTheme(state), cover);
});

test('same-song invalidation clears stale artwork and stopped playback clears colours', () => {
  let state = selectTheme(startSong(), { type: 'cover', videoId: 'song-a', theme: cover });
  state = selectTheme(state, { type: 'cover', videoId: 'song-a', theme: null });
  assert.equal(activeTheme(state), presets[0]);
  state = selectTheme(state, { type: 'cover', videoId: 'song-a', theme: cover });
  state = selectTheme(state, { type: 'track', videoId: null });
  assert.equal(activeTheme(state), presets[0]);
  assert.equal(selectTheme(state, { type: 'cover', videoId: null, theme: cover }), state);
});

test('invalid palettes cannot change the active theme', () => {
  const state = selectTheme(startSong(), { type: 'cover', videoId: 'song-a', theme: cover });
  for (const theme of [undefined, {}, { ...cover, name: 'arbitrary' },
    { ...cover, graphTheme: { ...cover.graphTheme, line1: 'red' } },
    { ...cover, uiTheme: { ...cover.uiTheme, background: '#12345678' } },
    { ...cover, uiTheme: { ...cover.uiTheme, primary: 'url(https://example.invalid)' } },
  ]) assert.equal(selectTheme(state, { type: 'cover', videoId: 'song-a', theme }), state);
});
