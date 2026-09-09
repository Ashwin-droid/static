import { useEffect, useRef, useState, type MutableRefObject } from 'react';
import * as THREE from 'three';
import { FrequencyHistory, MAX_HISTORY } from './protocol';
import { attachViewControls, type CameraControls } from './view-controls';
import { ThemeTransition, themeKey, mixColor } from './theme-transition';
import type { ThemePreset } from './themes';
interface Props {
  historyRef: MutableRefObject<FrequencyHistory>;
  captureRef: MutableRefObject<(() => string) | null>;
  viewRef: MutableRefObject<CameraControls>;
  restoreViewRef: MutableRefObject<((view: CameraControls) => void) | null>;
  theme: ThemePreset;
  displayedThemeRef: MutableRefObject<ThemePreset>;
  onThemeFrame: (theme: ThemePreset) => void;
  lineSpacing: number;
  numBands: number;
}

// Trace geometry, gradient, scale and orbit mapping adapted from echo-trace-visions.
export function Visualizer(props: Props) {
  const container = useRef<HTMLDivElement>(null);
  const current = useRef(props);
  current.current = props;
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const host = container.current;
    if (!host) return;
    let renderer: THREE.WebGLRenderer;
    try { renderer = new THREE.WebGLRenderer({ alpha: true, antialias: true }); }
    catch { setError('WebGL is unavailable. Enable hardware acceleration and reopen the visualiser.'); return; }
    setError(null);
    const scene = new THREE.Scene();
    const camera = new THREE.PerspectiveCamera(75, 1, 0.1, 1000);
    const group = new THREE.Group();
    group.scale.set(15, 1, 1);
    scene.add(group);
    host.appendChild(renderer.domElement);
    renderer.domElement.setAttribute('aria-label', 'Live frequency traces. Drag to rotate. With the canvas focused, W/S move forward/back, A/D move left/right, Q/E move down/up. Scroll to zoom. Double-click or press Home to reset.');
    const controls = attachViewControls(camera, renderer.domElement, view => { current.current.viewRef.current = view; });
    props.restoreViewRef.current = controls.restore;
    renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 2));
    renderer.setClearColor(0x000000, 0);
    let lines: THREE.Line<THREE.BufferGeometry, THREE.LineBasicMaterial>[] = [];
    let shape = '';
    let palette = '';
    let revision = -1;
    let previousHistory: FrequencyHistory | null = null;
    let frameId = 0;
    let stopped = false;
    const transition = new ThemeTransition(props.theme);
    const linearStart = new Float32Array(MAX_HISTORY * 3);
    const linearEnd = new Float32Array(MAX_HISTORY * 3);
    const startColor = new THREE.Color(), endColor = new THREE.Color();

    const disposeLines = () => {
      lines.forEach(line => { group.remove(line); line.geometry.dispose(); line.material.dispose(); });
      lines = [];
    };
    const resize = () => {
      const width = Math.max(1, host.clientWidth), height = Math.max(1, host.clientHeight);
      camera.aspect = width / height;
      camera.updateProjectionMatrix();
      renderer.setSize(width, height);
    };
    const observer = new ResizeObserver(resize);
    observer.observe(host);
    resize();

    const render = () => {
      const { theme, lineSpacing, numBands, historyRef } = current.current;
      const now = performance.now();
      transition.setTarget(theme, now);
      const displayed = transition.sample(now);
      current.current.displayedThemeRef.current = displayed;
      const nextShape = `${lineSpacing}/${numBands}`;
      if (nextShape !== shape) {
        disposeLines();
        for (let i = 0; i < numBands; i++) {
          const positions = new Float32Array(MAX_HISTORY * 3);
          for (let j = 0; j < MAX_HISTORY; j++) {
            positions[j * 3] = (j - MAX_HISTORY / 2) * 0.05;
            positions[j * 3 + 2] = (i - numBands / 2) * lineSpacing;
          }
          const geometry = new THREE.BufferGeometry();
          geometry.setAttribute('position', new THREE.BufferAttribute(positions, 3).setUsage(THREE.DynamicDrawUsage));
          geometry.setAttribute('color', new THREE.BufferAttribute(new Float32Array(MAX_HISTORY * 3), 3).setUsage(THREE.DynamicDrawUsage));
          const material = new THREE.LineBasicMaterial({ vertexColors: true, opacity: 0.8 + i / numBands * 0.2, transparent: true });
          const line = new THREE.Line(geometry, material);
          line.frustumCulled = false;
          lines.push(line);
          group.add(line);
        }
        shape = nextShape;
        palette = '';
        revision = -1;
      }
      const nextPalette = themeKey(displayed);
      if (nextPalette !== palette) {
        current.current.onThemeFrame(displayed);
        palette = nextPalette;
      }
      const history = historyRef.current;
      if (revision !== history.revision || previousHistory !== history) {
        // Convert each recorded time-column palette once, then share the values
        // across all bands. A later theme change never recolours earlier points.
        for (let j = 0; j < MAX_HISTORY; j++) {
          startColor.setHex(history.line1Colors[j]);
          endColor.setHex(history.line2Colors[j]);
          const offset = j * 3;
          linearStart[offset] = startColor.r; linearStart[offset + 1] = startColor.g; linearStart[offset + 2] = startColor.b;
          linearEnd[offset] = endColor.r; linearEnd[offset + 1] = endColor.g; linearEnd[offset + 2] = endColor.b;
        }
        lines.forEach((line, i) => {
          const positions = line.geometry.getAttribute('position') as THREE.BufferAttribute;
          const colors = line.geometry.getAttribute('color') as THREE.BufferAttribute;
          const row = history.samples[i];
          const band = i / numBands;
          for (let j = 0; j < MAX_HISTORY; j++) {
            positions.setY(j, (row?.[j] ?? 0) * 30);
            const offset = j * 3;
            colors.setXYZ(j,
              linearStart[offset] + (linearEnd[offset] - linearStart[offset]) * band,
              linearStart[offset + 1] + (linearEnd[offset + 1] - linearStart[offset + 1]) * band,
              linearStart[offset + 2] + (linearEnd[offset + 2] - linearStart[offset + 2]) * band);
          }
          positions.needsUpdate = true;
          colors.needsUpdate = true;
        });
        revision = history.revision;
        previousHistory = history;
      }
      controls.update();
      renderer.render(scene, camera);
    };
    const animate = () => {
      if (stopped) return;
      render();
      frameId = requestAnimationFrame(animate);
    };
    animate();
    const capture = () => {
      // Render and copy in the same task; no persistent WebGL drawing buffer is needed.
      render();
      const source = renderer.domElement;
      const scale = Math.min(1, 1280 / source.width, 720 / source.height);
      const canvas = document.createElement('canvas');
      canvas.width = Math.max(1, Math.round(source.width * scale));
      canvas.height = Math.max(1, Math.round(source.height * scale));
      const context = canvas.getContext('2d');
      if (!context) throw new Error('Could not capture the visualiser.');
      const shown = current.current.displayedThemeRef.current;
      const area = host.getBoundingClientRect();
      const app = host.parentElement?.getBoundingClientRect() ?? area;
      const position = (y: number) => Math.max(0, Math.min(1, (y - app.top) / Math.max(1, app.height)));
      const backdrop = context.createLinearGradient(0, 0, 0, canvas.height);
      backdrop.addColorStop(0, mixColor(shown.uiTheme.background, '#000000', position(area.top)));
      backdrop.addColorStop(1, mixColor(shown.uiTheme.background, '#000000', position(area.bottom)));
      context.fillStyle = backdrop;
      context.fillRect(0, 0, canvas.width, canvas.height);
      const tint = context.createLinearGradient(0, 0, 0, canvas.height);
      tint.addColorStop(0, `${shown.graphTheme.line1}10`);
      tint.addColorStop(1, `${shown.graphTheme.line2}05`);
      context.fillStyle = tint;
      context.fillRect(0, 0, canvas.width, canvas.height);
      context.drawImage(source, 0, 0, canvas.width, canvas.height);
      const png = canvas.toDataURL('image/png');
      canvas.width = canvas.height = 0;
      return png;
    };
    props.captureRef.current = capture;
    const lost = (event: Event) => {
      event.preventDefault();
      stopped = true;
      cancelAnimationFrame(frameId);
      current.current.captureRef.current = null;
      setError('The graphics context was lost. Reopen the visualiser to restore it.');
    };
    renderer.domElement.addEventListener('webglcontextlost', lost);
    return () => {
      stopped = true;
      cancelAnimationFrame(frameId);
      observer.disconnect();
      controls.dispose();
      current.current.restoreViewRef.current = null;
      renderer.domElement.removeEventListener('webglcontextlost', lost);
      if (current.current.captureRef.current === capture) current.current.captureRef.current = null;
      disposeLines();
      scene.clear();
      renderer.dispose();
      renderer.forceContextLoss();
      renderer.domElement.remove();
    };
  }, []);

  return <div ref={container} className="visualizer">
    {!error && <p className="gesture-hint"><span className="mouse-hint">Drag to rotate · WASD to move · Q/E down/up · Scroll to zoom · Double-click to reset</span><span className="touch-hint">Drag to rotate · Two fingers to move and zoom</span></p>}
    {error && <p className="canvas-error" role="alert">{error}</p>}
  </div>;
}
