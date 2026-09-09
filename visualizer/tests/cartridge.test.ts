import test from 'node:test';
import assert from 'node:assert/strict';
import { parseCartridge, readCartridge } from '../src/cartridge';
const data = () => ({ videoId: 'dQw4w9WgXcQ', uiTheme: { primary: '#112233', secondary: '#445566', background: '#000000' }, graphTheme: { line1: '#abcdef', line2: '#ffffff' }, cameraControls: { position: { x: -12, y: 20, z: 254 }, rotation: { yaw: 0.5, roll: 1.2, pitch: 0 }, distance: 60 }, fftSize: 30, lineSpacing: 2 });

test('recovered cartridge validates link, colours and quantized view before rendering', () => {
  const source = data();
  const result = parseCartridge(source)!;
  assert.equal(result.videoId, source.videoId);
  assert.deepEqual(result.theme.uiTheme, source.uiTheme);
  assert.equal(result.theme.name, '');
  assert.equal(result.bands, 30);
  assert.deepEqual(result.camera, source.cameraControls);
  for (const invalid of [null, {}, { ...source, videoId: 'https://evil.test' }, { ...source, lineSpacing: Infinity }, { ...source, fftSize: 31.5 }, { ...source, cameraControls: {} }, { ...source, uiTheme: { ...source.uiTheme, primary: 'url(evil)' } }]) assert.equal(parseCartridge(invalid), null);
  source.cameraControls.position.x = -257; assert.equal(parseCartridge(source), null);
});

test('import sends the file only to local Rust and propagates decode failures', async () => {
  const originalFetch = globalThis.fetch;
  const file = new File(['small-image'], 'cartridge.jpg', { type: 'image/jpeg' });
  const signal = new AbortController().signal;
  try {
    globalThis.fetch = (async (url, init) => {
      assert.equal(String(url), 'https://127.0.0.1:1234/cartridge/decode?token=secret');
      assert.equal(init?.body, file); assert.equal(init?.method, 'POST'); assert.equal(init?.signal, signal);
      return new Response(JSON.stringify(data()));
    }) as typeof fetch;
    assert.equal((await readCartridge(file, 'https://127.0.0.1:1234', '#token=secret', signal)).videoId, 'dQw4w9WgXcQ');
    globalThis.fetch = async () => new Response('Too damaged', { status: 422 });
    await assert.rejects(readCartridge(file, 'https://127.0.0.1:1234', '#token=secret', signal), /Too damaged/);
    await assert.rejects(readCartridge(file, 'https://127.0.0.1:1234', '', signal), /Open the visualiser/);
  } finally { globalThis.fetch = originalFetch; }
});
