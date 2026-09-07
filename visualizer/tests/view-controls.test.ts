import test from 'node:test';
import assert from 'node:assert/strict';
import { PerspectiveCamera, Vector3 } from 'three';
import { attachViewControls, initialCamera, type CameraControls } from '../src/view-controls';

class Canvas extends EventTarget {
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
    pageX: 100, pageY: 100, ctrlKey: false, metaKey: false, shiftKey: false, ...properties });
  canvas.dispatchEvent(event);
  return event;
}
function setup() {
  const canvas = new Canvas();
  const camera = new PerspectiveCamera(75, 1000 / 600, 0.1, 1000);
  let view: CameraControls = initialCamera();
  const controls = attachViewControls(camera, canvas as unknown as HTMLElement, next => { view = next; });
  const settle = () => { for (let i = 0; i < 120; i++) controls.update(); };
  return { canvas, camera, controls, settle, view: () => view };
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
