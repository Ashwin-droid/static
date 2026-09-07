import { PerspectiveCamera, Vector3 } from 'three';
import { OrbitControls } from 'three/examples/jsm/controls/OrbitControls.js';

/** Position is the orbit/pan target; distance completes the saved view. */
export interface CameraControls {
  position: { x: number; y: number; z: number };
  rotation: { yaw: number; roll: number; pitch: number };
  distance: number;
}

export const initialCamera = (): CameraControls => ({
  position: { x: 0, y: 0, z: 0 },
  rotation: { yaw: 0, roll: Math.PI * 80 / 180, pitch: 0 },
  distance: 60,
});

export function attachViewControls(camera: PerspectiveCamera, canvas: HTMLElement,
  onChange: (view: CameraControls) => void, clock: () => number = () => performance.now()) {
  const initial = initialCamera();
  camera.position.set(0, initial.distance * Math.cos(initial.rotation.roll), initial.distance * Math.sin(initial.rotation.roll));
  const controls = new OrbitControls(camera, canvas);
  controls.enableDamping = true;
  controls.dampingFactor = 0.12;
  controls.minDistance = 5;
  controls.maxDistance = 600;
  controls.minPolarAngle = 0.01;
  controls.maxPolarAngle = Math.PI - 0.01;
  controls.screenSpacePanning = true;
  controls.listenToKeyEvents(canvas);
  controls.saveState();
  const held = new Set<string>();
  const movementKeys = new Set(['KeyW', 'KeyA', 'KeyS', 'KeyD', 'KeyQ', 'KeyE']);
  const forward = new Vector3(), right = new Vector3(), movement = new Vector3();
  let previousTime = clock();
  let disposed = false;
  const document = canvas.ownerDocument;
  const window = document.defaultView;
  const movementKey = (event: KeyboardEvent) => {
    const code = event.code || `Key${event.key.toUpperCase()}`;
    return movementKeys.has(code) ? code : null;
  };
  const stopMoving = () => { held.clear(); previousTime = clock(); };
  const visibilityChanged = () => { if (document.hidden) stopMoving(); };

  const sync = () => onChange({
    position: { x: controls.target.x, y: controls.target.y, z: controls.target.z },
    rotation: { yaw: controls.getAzimuthalAngle(), roll: controls.getPolarAngle(), pitch: 0 },
    distance: controls.getDistance(),
  });
  const start = () => canvas.classList.add('dragging');
  const end = () => canvas.classList.remove('dragging');
  const focus = () => canvas.focus({ preventScroll: true });
  const reset = () => {
    stopMoving();
    // Drain inertial deltas before restoring the saved pose; otherwise an
    // immediate reset after dragging would keep drifting away from the origin.
    controls.enableDamping = false;
    controls.update();
    controls.reset();
    controls.enableDamping = true;
    sync();
  };
  const doubleClick = (event: MouseEvent) => { event.preventDefault(); reset(); };
  const keyDown = (event: KeyboardEvent) => {
    if (event.key === 'Home') { event.preventDefault(); reset(); }
    if (event.target !== canvas || event.ctrlKey || event.metaKey || event.altKey || event.isComposing) return;
    const key = movementKey(event);
    if (!key) return;
    event.preventDefault();
    if (held.size === 0) previousTime = clock();
    held.add(key);
  };
  const keyUp = (event: KeyboardEvent) => {
    const key = movementKey(event);
    if (key) held.delete(key);
  };
  canvas.tabIndex = 0;
  canvas.addEventListener('pointerdown', focus);
  canvas.addEventListener('dblclick', doubleClick);
  canvas.addEventListener('keydown', keyDown);
  canvas.addEventListener('keyup', keyUp);
  canvas.addEventListener('blur', stopMoving);
  window?.addEventListener('blur', stopMoving);
  document.addEventListener('visibilitychange', visibilityChanged);
  document.addEventListener('fullscreenchange', stopMoving);
  controls.addEventListener('change', sync);
  controls.addEventListener('start', start);
  controls.addEventListener('end', end);
  sync();
  focus();

  return {
    update: () => {
      if (disposed) return false;
      const now = clock();
      const seconds = Math.max(0, Math.min(0.1, (now - previousTime) / 1000));
      previousTime = now;
      const changed = controls.update();
      const x = Number(held.has('KeyD')) - Number(held.has('KeyA'));
      const z = Number(held.has('KeyW')) - Number(held.has('KeyS'));
      const y = Number(held.has('KeyE')) - Number(held.has('KeyQ'));
      camera.getWorldDirection(forward);
      right.set(1, 0, 0).applyQuaternion(camera.quaternion);
      movement.copy(forward).multiplyScalar(z).addScaledVector(right, x);
      movement.y += y; // Q/E move vertically in the scene, matching Blender's walk binds.
      const moving = movement.lengthSq() > 0 && seconds > 0;
      if (moving) {
        movement.normalize().multiplyScalar(30 * seconds);
        // Translate eye and target together: movement is not a zoom or rotation.
        camera.position.add(movement);
        controls.target.add(movement);
        camera.updateMatrixWorld();
      }
      // OrbitControls suppresses tiny 'change' events once damping settles.
      // Capture metadata must still describe the exact rendered frame.
      sync();
      return changed || moving;
    },
    reset,
    dispose: () => {
      disposed = true;
      stopMoving();
      canvas.removeEventListener('pointerdown', focus);
      canvas.removeEventListener('dblclick', doubleClick);
      canvas.removeEventListener('keydown', keyDown);
      canvas.removeEventListener('keyup', keyUp);
      canvas.removeEventListener('blur', stopMoving);
      window?.removeEventListener('blur', stopMoving);
      document.removeEventListener('visibilitychange', visibilityChanged);
      document.removeEventListener('fullscreenchange', stopMoving);
      controls.removeEventListener('change', sync);
      controls.removeEventListener('start', start);
      controls.removeEventListener('end', end);
      controls.dispose();
      end();
    },
  };
}
