import { test } from 'node:test';
import assert from 'node:assert/strict';
import { FrequencyHistory, MAX_HISTORY, parseMessage, socketUrl, downloadUrl, reconnectDelay, type AudioFrame } from '../src/protocol.ts';
import { presets } from '../src/themes.ts';

const frame = (sequence: number, generation = 1, value = 0.5): AudioFrame => ({
  generation, sequence, sample_rate: 48_000, bands: Array(30).fill(value), rms: 0.2, peak: 0.8,
});

test('parses Rust analysis frames and rejects invalid/big frames', () => {
  assert.equal(parseMessage(JSON.stringify({ type: 'audio', frame: frame(1) }))?.type, 'audio');
  for (const invalid of [
    { ...frame(1), bands: Array(129).fill(0) },
    { ...frame(1), bands: Array(7).fill(0) },
    { ...frame(1), bands: Array(30).fill(-0.1) },
    { ...frame(1), bands: Array(30).fill(1.1) },
    { ...frame(1), generation: -1 },
    { ...frame(1), sequence: 1.1 },
    { ...frame(1), sample_rate: 0 },
    { ...frame(1), rms: '1' },
  ]) assert.equal(parseMessage(JSON.stringify({ type: 'audio', frame: invalid })), null);
  for (const raw of ['no json', '{}', 'null', ' '.repeat(2_000_001), 12, new Uint8Array(8)]) assert.equal(parseMessage(raw), null);
});

test('extracts only needed daemon snapshot fields and preserves boosted volume', () => {
  const state = { current_track: { video_id: 'abc', title: 'Song', artist: 'Artist', thumb_url: 'https://example.invalid/art' }, paused: false, volume: 4, max_volume: 5, position_millis: 123, duration_millis: 999, queue: [{ title: 'next' }] };
  const message = parseMessage(JSON.stringify({ type: 'state', state }));
  assert.equal(message?.type, 'state');
  if (message?.type === 'state') {
    assert.equal(message.state.max_volume, 5);
    assert.equal(message.state.volume, 4);
    assert.equal('queue' in message.state, false);
    assert.equal('thumb_url' in message.state.current_track!, false);
  }
  assert.equal(parseMessage(JSON.stringify({ type: 'state', state: { ...state, max_volume: 500 } })), null);
});

test('cover theme protocol accepts only five RGB colours and permits explicit invalidation', () => {
  const theme = { ...presets[0], name: 'Cover art' };
  const message = { type: 'theme', video_id: 'song', theme };
  assert.deepEqual(parseMessage(JSON.stringify({ ...message, theme: { ...theme, image: 'unused', uiTheme: { ...theme.uiTheme, css: 'unused' } } })), message);
  for (const video_id of ['song', null]) {
    const reset = { type: 'theme', video_id, theme: null };
    assert.deepEqual(parseMessage(JSON.stringify(reset)), reset);
  }
  for (const invalid of [
    { ...message, video_id: null }, { ...message, video_id: 12 }, { ...message, theme: undefined },
    { ...message, theme: { ...theme, name: 'x'.repeat(10_000) } },
    ...['#123', '#12345678', '#xyzxyz', '#ffffff\n', 'red', 123, null, 'x'.repeat(10_000)]
      .map(primary => ({ ...message, theme: { ...theme, uiTheme: { ...theme.uiTheme, primary } } })),
    { ...message, theme: { ...theme, graphTheme: { line1: '#abcdef' } } },
  ]) assert.equal(parseMessage(JSON.stringify(invalid)), null);
});

test('history is bounded at 350 frames, advances only on new sequence, and freezes when paused', () => {
  const history = new FrequencyHistory(30);
  const allocations = [...history.samples];
  for (let seq = 0; seq < 20_000; seq++) history.append(frame(seq, 1, seq / 20_000), false);
  assert.equal(history.samples.length, 30);
  assert.equal(history.samples[0].length, MAX_HISTORY);
  assert.equal(history.samples[0], allocations[0]);
  assert.ok(Math.abs(history.samples[0][0] - (20_000 - MAX_HISTORY) / 20_000) < 0.000001);
  const before = history.samples[0].slice();
  assert.equal(history.append(frame(19_999), false), false);
  assert.equal(history.append(frame(19_000), false), false);
  assert.equal(history.append(frame(20_000), true), false);
  assert.deepEqual(history.samples[0], before);
  assert.equal(history.append(frame(20_000), false), false);
  assert.equal(history.append(frame(20_001), false), true);
});

test('new generation clears previous song, including on pause; stale generations cannot return', () => {
  const history = new FrequencyHistory(30);
  history.append(frame(10), false);
  history.append(frame(11), false);
  history.append(frame(0, 2), false);
  assert.equal(history.samples[0][MAX_HISTORY - 2], 0);
  assert.equal(history.samples[0][MAX_HISTORY - 1], 0.5);
  assert.equal(history.append(frame(12, 1), false), false);
  history.append(frame(0, 3), true);
  assert.ok(history.samples.every(row => row.every(value => value === 0)));
});

test('configuration mismatch never mixes frequency scales', () => {
  const history = new FrequencyHistory(8);
  assert.equal(history.append(frame(0), false), false);
  assert.equal(history.append({ ...frame(1), bands: Array(8).fill(0.7) }, false), true);
  assert.equal(history.samples[0].length, 350);
  for (const n of [0, 7, 129, 1.5, NaN]) assert.throws(() => new FrequencyHistory(n), RangeError);
});

test('socket token stays in same-host WSS and is correctly encoded', () => {
  assert.equal(socketUrl('127.0.0.1:1234', '#token=a%2Bb%26c'), 'wss://127.0.0.1:1234/ws?token=a%2Bb%26c');
  assert.equal(socketUrl('127.0.0.1:1234', ''), null);
});

test('cartridge download only permits local HTTPS cartridge endpoints', () => {
  const origin = 'https://127.0.0.1:1234';
  assert.equal(downloadUrl('/cartridges/abc?token=xyz', origin), `${origin}/cartridges/abc?token=xyz`);
  for (const url of ['https://example.invalid/cartridges/a', 'javascript:alert(1)', 'http://127.0.0.1:1234/cartridges/a', '/ws', '//example.invalid/cartridges/a']) assert.equal(downloadUrl(url, origin), null);
});

test('reconnect backoff is bounded', () => {
  assert.deepEqual([0, 1, 2, 3, 4, 5, 99].map(reconnectDelay), [500, 1000, 2000, 4000, 8000, 15000, 15000]);
});


test('cartridge status distinguishes volatile memory from saved disk files', () => {
  const ready = { type: 'cartridge_ready', id: 'id', filename: 'song.png', url: '/cartridges/id', storage: 'memory', path: null };
  for (const message of [ready, { ...ready, path: '/music/staticfs/cartridges/song.png' }, { ...ready, storage: 'disk', path: '/music/cartridges/song.png' }]) {
    assert.deepEqual(parseMessage(JSON.stringify(message)), message);
  }
  for (const message of [{ ...ready, storage: 'disk' }, { ...ready, storage: undefined }, { ...ready, storage: 'cloud' }, { ...ready, path: 123 }, { ...ready, path: undefined }]) {
    assert.equal(parseMessage(JSON.stringify(message)), null);
  }
});
