import test from 'node:test';
import assert from 'node:assert/strict';
import { PerspectiveCamera, Vector3 } from 'three';
import { attachViewControls, initialCamera, type CameraControls } from '../src/view-controls';

class Document extends EventTarget {
  hidden = false;
  defaultView = new EventTarget();
}
class Canvas extends EventTarget {
  ownerDocument = new Document();
  clientWidth = 1000;
  clientHeight = 600;
  style = { touchAction: 'auto' };
  tabIndex = -1;
  focused = false;
  classes = new Set<string>();
  classList = { add: (name: string) => this.classes.add(name), remove: (name: string) => this.classes.delete(name) };
  captures = new Set<number>();
  setPointerCapture(id: number) { this.captures.add(id); }
  releasePointerCapture(id: number) { this.captures.delete(id); }
  focus() { this.focused = true; }
}
function input(canvas: Canvas, type: string, properties: Record<string, unknown> = {}) {
  const event = new Event(type, { cancelable: true });
  Object.assign(event, { pointerId: 1, pointerType: 'mouse', button: 0, clientX: 100, clientY: 100,
    pageX: 100, pageY: 100, key: '', ctrlKey: false, metaKey: false, shiftKey: false, ...properties });
  canvas.dispatchEvent(event);
  return event;
}
function setup() {
  const canvas = new Canvas();
  const camera = new PerspectiveCamera(75, 1000 / 600, 0.1, 1000);
  let view: CameraControls = initialCamera();
  let now = 0;
  const controls = attachViewControls(camera, canvas as unknown as HTMLElement, next => { view = next; }, () => now);
  const advance = (milliseconds: number) => { now += milliseconds; controls.update(); };
  const settle = () => { for (let i = 0; i < 120; i++) controls.update(); };
  return { canvas, camera, controls, settle, advance, view: () => view };
}
function near(a: number, b: number) { assert.ok(Math.abs(a - b) < 1e-6, `${a} != ${b}`); }

test('drag rotates directly and the captured view matches the camera pose', () => {
  const s = setup();
  const before = s.camera.position.clone();
  input(s.canvas, 'pointerdown');
  assert.ok(s.canvas.focused);
  assert.ok(s.canvas.classes.has('dragging'));
  input(s.canvas, 'pointermove', { clientX: 250, clientY: 150 });
  input(s.canvas, 'pointerup', { clientX: 250, clientY: 150 });
  s.settle();
  assert.ok(s.camera.position.distanceTo(before) > 10);
  assert.ok(!s.canvas.classes.has('dragging'));
  const view = s.view();
  const actual = new Vector3(view.position.x, view.position.y, view.position.z).add(new Vector3(
    view.distance * Math.sin(view.rotation.roll) * Math.sin(view.rotation.yaw),
    view.distance * Math.cos(view.rotation.roll),
    view.distance * Math.sin(view.rotation.roll) * Math.cos(view.rotation.yaw),
  ));
  assert.ok(actual.distanceTo(s.camera.position) < 1e-6);
  s.controls.dispose();
});

test('right-drag and shift-drag pan without changing the orbit angle', () => {
  for (const modifiers of [{ button: 2 }, { shiftKey: true }]) {
    const s = setup();
    input(s.canvas, 'pointerdown', modifiers);
    input(s.canvas, 'pointermove', { ...modifiers, clientX: 170, clientY: 130 });
    input(s.canvas, 'pointerup', modifiers);
    s.settle();
    assert.ok(Math.abs(s.view().position.x) > 1);
    near(s.view().rotation.yaw, 0);
    near(s.view().rotation.roll, initialCamera().rotation.roll);
    s.controls.dispose();
  }
});

test('wheel zoom is bounded and double-click clears inertia for an exact reset', () => {
  const s = setup();
  assert.ok(input(s.canvas, 'wheel', { deltaY: -100 }).defaultPrevented);
  assert.ok(s.view().distance < 60);
  for (let i = 0; i < 200; i++) input(s.canvas, 'wheel', { deltaY: -100 });
  near(s.view().distance, 5);
  for (let i = 0; i < 300; i++) input(s.canvas, 'wheel', { deltaY: 100 });
  near(s.view().distance, 600);
  input(s.canvas, 'pointerdown');
  input(s.canvas, 'pointermove', { clientX: 300 });
  input(s.canvas, 'pointerup');
  input(s.canvas, 'dblclick');
  s.settle();
  near(s.view().distance, 60);
  near(s.view().rotation.yaw, 0);
  near(s.view().rotation.roll, initialCamera().rotation.roll);
  near(s.view().position.x, 0);
  s.controls.dispose();
});

test('two-finger drag pans and pinches to zoom; cancellation and disposal remove controls', () => {
  const s = setup();
  input(s.canvas, 'pointerdown', { pointerType: 'touch', pointerId: 1, pageX: 100, pageY: 100 });
  input(s.canvas, 'pointerdown', { pointerType: 'touch', pointerId: 2, pageX: 200, pageY: 100 });
  input(s.canvas, 'pointermove', { pointerType: 'touch', pointerId: 2, pageX: 260, pageY: 130 });
  input(s.canvas, 'pointercancel', { pointerType: 'touch', pointerId: 1 });
  input(s.canvas, 'pointercancel', { pointerType: 'touch', pointerId: 2 });
  s.settle();
  assert.ok(s.view().distance < 60);
  assert.ok(Math.abs(s.view().position.x) > 1);
  input(s.canvas, 'keydown', { key: 'Home' });
  near(s.view().distance, 60);
  s.controls.dispose();
  const before = s.camera.position.clone();
  input(s.canvas, 'pointerdown');
  input(s.canvas, 'pointermove', { clientX: 500 });
  input(s.canvas, 'wheel', { deltaY: -100 });
  assert.ok(s.camera.position.equals(before));
});

test('WASD moves relative to the view while Q/E moves vertically, without zooming', () => {
  for (const [key, axis, direction] of [
    ['w', 'forward', 1], ['s', 'forward', -1], ['a', 'right', -1], ['d', 'right', 1], ['q', 'vertical', -1], ['e', 'vertical', 1],
  ] as const) {
    const s = setup();
    const start = s.camera.position.clone();
    const expected = axis === 'forward' ? s.camera.getWorldDirection(new Vector3())
      : axis === 'right' ? new Vector3(1, 0, 0).applyQuaternion(s.camera.quaternion) : new Vector3(0, 1, 0);
    expected.multiplyScalar(direction * 30);
    assert.ok(input(s.canvas, 'keydown', { key }).defaultPrevented);
    for (let i = 0; i < 60; i++) s.advance(1000 / 60);
    input(s.canvas, 'keyup', { key });
    assert.ok(s.camera.position.clone().sub(start).distanceTo(expected) < 1e-6);
    near(s.view().distance, 60);
    const stopped = s.camera.position.clone();
    s.advance(100);
    assert.ok(s.camera.position.distanceTo(stopped) < 1e-6);
    s.controls.dispose();
  }
});

test('keyboard motion follows mouse rotation, is frame-rate independent and normalizes diagonals', () => {
  for (const fps of [30, 60, 120]) {
    const s = setup();
    input(s.canvas, 'pointerdown');
    input(s.canvas, 'pointermove', { clientX: 260, clientY: 140 });
    input(s.canvas, 'pointerup');
    s.settle();
    const start = s.camera.position.clone();
    const expected = s.camera.getWorldDirection(new Vector3()).multiplyScalar(30);
    input(s.canvas, 'keydown', { key: 'w', code: 'KeyW' });
    for (let i = 0; i < fps; i++) s.advance(1000 / fps);
    assert.ok(s.camera.position.clone().sub(start).distanceTo(expected) < 1e-4);
    input(s.canvas, 'keydown', { key: 'd', code: 'KeyD' });
    const diagonalStart = s.camera.position.clone();
    for (let i = 0; i < fps; i++) s.advance(1000 / fps);
    near(s.camera.position.distanceTo(diagonalStart), 30);
    s.controls.dispose();
  }
});

test('blur, hidden windows, reset and disposal stop movement; shortcuts do not move the view', () => {
  for (const stop of ['canvas', 'window', 'hidden', 'reset', 'dispose']) {
    const s = setup();
    input(s.canvas, 'keydown', { key: 'w' });
    s.advance(16);
    if (stop === 'canvas') s.canvas.dispatchEvent(new Event('blur'));
    if (stop === 'window') s.canvas.ownerDocument.defaultView.dispatchEvent(new Event('blur'));
    if (stop === 'hidden') { s.canvas.ownerDocument.hidden = true; s.canvas.ownerDocument.dispatchEvent(new Event('visibilitychange')); }
    if (stop === 'reset') s.controls.reset();
    if (stop === 'dispose') s.controls.dispose();
    const position = s.camera.position.clone();
    s.advance(10_000);
    assert.ok(s.camera.position.distanceTo(position) < 1e-6);
    s.controls.dispose();
  }
  const s = setup();
  for (const modifier of ['ctrlKey', 'altKey', 'metaKey', 'isComposing']) {
    assert.ok(!input(s.canvas, 'keydown', { key: 's', [modifier]: true }).defaultPrevented);
    s.advance(100);
  }
  near(new Vector3(s.view().position.x, s.view().position.y, s.view().position.z).length(), 0);
  input(s.canvas, 'keydown', { key: 'w' });
  s.advance(10_000);
  near(new Vector3(s.view().position.x, s.view().position.y, s.view().position.z).length(), 3);
  s.controls.dispose();
});
