import test from 'node:test';
import assert from 'node:assert/strict';
import { FullscreenView, type FullscreenState } from '../src/fullscreen';

class FakeDocument extends EventTarget {
  fullscreenElement: HTMLElement | null = null;
  async exitFullscreen() { this.fullscreenElement = null; this.dispatchEvent(new Event('fullscreenchange')); }
}
function setup(reject = false) {
  const document = new FakeDocument();
  let requests = 0;
  const host = { ownerDocument: document, requestFullscreen: async () => {
    requests++;
    if (reject) throw new Error('Fullscreen denied');
    document.fullscreenElement = host as unknown as HTMLElement;
    document.dispatchEvent(new Event('fullscreenchange'));
  } };
  let state: FullscreenState = { active: false, controls: true };
  const errors: string[] = [];
  const controller = new FullscreenView(host as unknown as HTMLElement, next => { state = next; }, error => errors.push(error));
  return { controller, document, state: () => state, errors, requests: () => requests };
}
test('fullscreen hides controls, corner toggle reveals them, exit restores them', async () => {
  const s = setup();
  await s.controller.toggle();
  assert.deepEqual(s.state(), { active: true, controls: false });
  s.controller.toggleControls();
  assert.deepEqual(s.state(), { active: true, controls: true });
  s.controller.toggleControls();
  assert.deepEqual(s.state(), { active: true, controls: false });
  await s.controller.toggle();
  assert.deepEqual(s.state(), { active: false, controls: true });
  await s.controller.toggle();
  assert.deepEqual(s.state(), { active: true, controls: false });
  s.controller.dispose();
});
test('failed entry leaves controls visible and reports the actual error', async () => {
  const s = setup(true);
  await s.controller.toggle();
  assert.deepEqual(s.state(), { active: false, controls: true });
  assert.deepEqual(s.errors, ['Fullscreen denied']);
  s.controller.dispose();
});
test('external Escape exits fullscreen and disposal removes keyboard handlers', async () => {
  const s = setup();
  const f11 = new Event('keydown', { cancelable: true }); Object.assign(f11, { key: 'F11' });
  s.document.dispatchEvent(f11);
  assert.ok(f11.defaultPrevented);
  assert.deepEqual(s.state(), { active: true, controls: false });
  const escape = new Event('keydown'); Object.assign(escape, { key: 'Escape' });
  s.document.dispatchEvent(escape);
  assert.deepEqual(s.state(), { active: false, controls: true });
  s.controller.dispose();
  s.document.dispatchEvent(f11);
  assert.equal(s.requests(), 1);
});
