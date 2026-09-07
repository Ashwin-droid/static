import { PerspectiveCamera } from 'three';
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
  onChange: (view: CameraControls) => void) {
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

  const sync = () => onChange({
    position: { x: controls.target.x, y: controls.target.y, z: controls.target.z },
    rotation: { yaw: controls.getAzimuthalAngle(), roll: controls.getPolarAngle(), pitch: 0 },
    distance: controls.getDistance(),
  });
  const start = () => canvas.classList.add('dragging');
  const end = () => canvas.classList.remove('dragging');
  const focus = () => canvas.focus({ preventScroll: true });
  const reset = () => {
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
  };
  canvas.tabIndex = 0;
  canvas.addEventListener('pointerdown', focus);
  canvas.addEventListener('dblclick', doubleClick);
  canvas.addEventListener('keydown', keyDown);
  controls.addEventListener('change', sync);
  controls.addEventListener('start', start);
  controls.addEventListener('end', end);
  sync();

  return {
    update: () => {
      const changed = controls.update();
      // OrbitControls suppresses tiny 'change' events once damping settles.
      // Capture metadata must still describe the exact rendered frame.
      sync();
      return changed;
    },
    reset,
    dispose: () => {
      canvas.removeEventListener('pointerdown', focus);
      canvas.removeEventListener('dblclick', doubleClick);
      canvas.removeEventListener('keydown', keyDown);
      controls.removeEventListener('change', sync);
      controls.removeEventListener('start', start);
      controls.removeEventListener('end', end);
      controls.dispose();
      end();
    },
  };
}
