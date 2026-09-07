import { useEffect, useRef, useState, type MutableRefObject } from 'react';
import * as THREE from 'three';
import { FrequencyHistory, MAX_HISTORY } from './protocol';
import { attachViewControls, type CameraControls } from './view-controls';
interface Props {
  historyRef: MutableRefObject<FrequencyHistory>;
  captureRef: MutableRefObject<(() => string) | null>;
  viewRef: MutableRefObject<CameraControls>;
  graphTheme: { line1: string; line2: string };
  background: string;
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
    renderer.domElement.setAttribute('aria-label', 'Live frequency traces. Drag to rotate, Shift or right drag to pan, scroll to zoom. Double-click or press Home to reset.');
    const controls = attachViewControls(camera, renderer.domElement, view => { current.current.viewRef.current = view; });
    renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 2));
    renderer.setClearColor(0x000000, 0);
    let lines: THREE.Line<THREE.BufferGeometry, THREE.LineBasicMaterial>[] = [];
    let shape = '';
    let palette = '';
    let revision = -1;
    let previousHistory: FrequencyHistory | null = null;
    let frameId = 0;
    let stopped = false;

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
      const { graphTheme, lineSpacing, numBands, historyRef } = current.current;
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
          const material = new THREE.LineBasicMaterial({ opacity: 0.8 + i / numBands * 0.2, transparent: true });
          const line = new THREE.Line(geometry, material);
          line.frustumCulled = false;
          lines.push(line);
          group.add(line);
        }
        shape = nextShape;
        palette = '';
        revision = -1;
      }
      const nextPalette = `${graphTheme.line1}/${graphTheme.line2}`;
      if (nextPalette !== palette) {
        lines.forEach((line, i) => line.material.color.set(graphTheme.line1).lerp(new THREE.Color(graphTheme.line2), i / numBands));
        palette = nextPalette;
      }
      const history = historyRef.current;
      if (revision !== history.revision || previousHistory !== history) {
        lines.forEach((line, i) => {
          const positions = line.geometry.getAttribute('position') as THREE.BufferAttribute;
          const row = history.samples[i];
          for (let j = 0; j < MAX_HISTORY; j++) positions.setY(j, (row?.[j] ?? 0) * 30);
          positions.needsUpdate = true;
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
      context.fillStyle = current.current.background;
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
      renderer.domElement.removeEventListener('webglcontextlost', lost);
      if (current.current.captureRef.current === capture) current.current.captureRef.current = null;
      disposeLines();
      scene.clear();
      renderer.dispose();
      renderer.forceContextLoss();
      renderer.domElement.remove();
    };
  }, []);

  return <div ref={container} className="visualizer" style={{ background: `linear-gradient(to bottom, ${props.graphTheme.line1}10, ${props.graphTheme.line2}05)` }}>
    {!error && <p className="gesture-hint"><span className="mouse-hint">Drag to rotate · Shift/right-drag to move · Scroll to zoom · Double-click to reset</span><span className="touch-hint">Drag to rotate · Two fingers to move and zoom</span></p>}
    {error && <p className="canvas-error" role="alert">{error}</p>}
  </div>;
}
