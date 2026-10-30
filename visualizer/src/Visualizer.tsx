import { useEffect, useRef, useState, type MutableRefObject } from 'react';
import * as THREE from 'three';
import { FrequencyHistory, MAX_HISTORY } from './protocol';
import { attachViewControls, type CameraControls } from './view-controls';
import { ThemeTransition, themeKey, mixColor } from './theme-transition';
import { describeBand, pickPolyline, type Isolation } from './isolation';
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
  isolation: Isolation;
  /** Paint an inclusive band range lit or dark; fires during a drag. */
  onPaint: (from: number, to: number, value: boolean) => void;
  /** The drag ended; flush the selection to Static. */
  onPaintEnd: () => void;
}

// Tube brightness as a multiplier over the theme's vertex colours. Lit tubes
// exceed the bloom gate and glow; dark ones keep their tint but stop emitting.
const LIT = 1.6, LIT_HOVER = 2.4, DARK = 0.13, DARK_HOVER = 0.4;
const REFLECTION = 0.8, REFLECTION_OPACITY = 0.3;

/**
 * Neon pipeline, written by hand rather than with UnrealBloomPass: this WebKit
 * renders that pass's 1/32-resolution mips as hard blocks, so the glow here
 * never leaves half resolution and gets its width from widening blur steps.
 * Everything runs in half-float targets when the GPU can render into them,
 * and the final pass computes the page backdrop in linear light, encodes to
 * sRGB and dithers, so dark gradients and halos never band.
 */
const QUAD_VERTEX = `varying vec2 vUv; void main() { vUv = uv; gl_Position = vec4(position.xy, 0.0, 1.0); }`;
const BRIGHT_FRAGMENT = `
  uniform sampler2D tDiffuse; uniform vec2 texel; uniform float threshold;
  varying vec2 vUv;
  void main() {
    vec3 c = texture2D(tDiffuse, vUv + texel * vec2(-0.5, -0.5)).rgb + texture2D(tDiffuse, vUv + texel * vec2(0.5, -0.5)).rgb
           + texture2D(tDiffuse, vUv + texel * vec2(-0.5, 0.5)).rgb + texture2D(tDiffuse, vUv + texel * vec2(0.5, 0.5)).rgb;
    c *= 0.25;
    float lum = dot(c, vec3(0.2126, 0.7152, 0.0722));
    gl_FragColor = vec4(c * smoothstep(threshold, threshold + 0.12, lum), 1.0);
  }`;
const BLUR_FRAGMENT = `
  uniform sampler2D tDiffuse; uniform vec2 step;
  varying vec2 vUv;
  void main() {
    vec3 c = texture2D(tDiffuse, vUv).rgb * 0.2270270270;
    c += (texture2D(tDiffuse, vUv + step * 1.3846153846).rgb + texture2D(tDiffuse, vUv - step * 1.3846153846).rgb) * 0.3162162162;
    c += (texture2D(tDiffuse, vUv + step * 3.2307692308).rgb + texture2D(tDiffuse, vUv - step * 3.2307692308).rgb) * 0.0702702703;
    gl_FragColor = vec4(c, 1.0);
  }`;
const COMPOSITE_FRAGMENT = `
  uniform sampler2D tScene, tBloom; uniform vec3 topColor, bottomColor, tintTop, tintBottom; uniform float strength;
  varying vec2 vUv;
  float hash(vec2 p) { return fract(sin(dot(p, vec2(12.9898, 78.233))) * 43758.5453); }
  void main() {
    vec4 scene = texture2D(tScene, vUv);
    vec3 bloom = texture2D(tBloom, vUv).rgb;
    float v = 1.0 - vUv.y;
    vec3 backdrop = mix(topColor, bottomColor, v);
    backdrop = mix(backdrop, mix(tintTop, tintBottom, v), mix(0.0627, 0.0196, v));
    // Lines land with alpha; the glow is added over the backdrop.
    vec3 color = max(backdrop * (1.0 - scene.a) + scene.rgb + bloom * strength, 0.0);
    vec3 srgb = mix(color * 12.92, 1.055 * pow(color, vec3(1.0 / 2.4)) - 0.055, step(0.0031308, color));
    float noise = hash(gl_FragCoord.xy) / 255.0 - 0.5 / 255.0;
    gl_FragColor = vec4(clamp(srgb + noise, 0.0, 1.0), 1.0);
  }`;
const BLOOM_STRENGTH = 1.0, BLOOM_THRESHOLD = 0.06;
/** Blur radii in half-resolution pixels; each is a separable pass pair. */
const BLOOM_STEPS = [1, 2, 4, 8, 14];

const PICK_TOLERANCE = { mouse: 16, pen: 16, touch: 28 } as const;

type LineMesh = THREE.Line<THREE.BufferGeometry, THREE.LineBasicMaterial>;

/** The CSS backdrop behind the canvas, drawn into a 2D context: shared by captures and the isolation background. */
function paintBackdrop(context: CanvasRenderingContext2D, width: number, height: number, shown: ThemePreset, area: DOMRect, app: DOMRect) {
  const position = (y: number) => Math.max(0, Math.min(1, (y - app.top) / Math.max(1, app.height)));
  const backdrop = context.createLinearGradient(0, 0, 0, height);
  backdrop.addColorStop(0, mixColor(shown.uiTheme.background, '#000000', position(area.top)));
  backdrop.addColorStop(1, mixColor(shown.uiTheme.background, '#000000', position(area.bottom)));
  context.fillStyle = backdrop;
  context.fillRect(0, 0, width, height);
  const tint = context.createLinearGradient(0, 0, 0, height);
  tint.addColorStop(0, `${shown.graphTheme.line1}10`);
  tint.addColorStop(1, `${shown.graphTheme.line2}05`);
  context.fillStyle = tint;
  context.fillRect(0, 0, width, height);
}

// Trace geometry, gradient, scale and orbit mapping adapted from echo-trace-visions.
export function Visualizer(props: Props) {
  const container = useRef<HTMLDivElement>(null);
  const current = useRef(props);
  current.current = props;
  const [error, setError] = useState<string | null>(null);
  const [hovered, setHovered] = useState<number | null>(null);

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
    // Fake ray tracing: the lit tubes mirror into a dark glossy floor that goes matte with distance.
    const mirror = new THREE.Group();
    mirror.scale.set(15, -1, 1);
    mirror.visible = false;
    scene.add(mirror);
    // Reflections sink into the backdrop with distance; only they are fogged.
    const reflectionFog = new THREE.FogExp2(0x000000, 0.011);
    host.appendChild(renderer.domElement);
    const canvas = renderer.domElement;
    canvas.setAttribute('aria-label', 'Live frequency traces. Drag to rotate. With the canvas focused, W/S move forward/back, A/D move left/right, Q/E move down/up. Scroll to zoom. Double-click or press Home to reset. With band isolation on, click a tube to light it and drag across tubes to paint.');

    // --- Band picking. Registered before the orbit controls so a hit on a tube
    // can stop the drag from becoming a rotation; a miss falls through to orbiting.
    const projected = new THREE.Vector3();
    const pick = (clientX: number, clientY: number, tolerance: number): number | null => {
      const { numBands } = current.current;
      const rect = canvas.getBoundingClientRect();
      if (rect.width === 0 || rect.height === 0 || lines.length !== numBands) return null;
      const px = clientX - rect.left, py = clientY - rect.top;
      camera.updateMatrixWorld();
      group.updateMatrixWorld();
      const matrix = group.matrixWorld;
      return pickPolyline(numBands, MAX_HISTORY, px, py, tolerance, (band, column, out) => {
        const positions = lines[band].geometry.getAttribute('position') as THREE.BufferAttribute;
        projected.set(positions.getX(column), positions.getY(column), positions.getZ(column)).applyMatrix4(matrix).project(camera);
        if (projected.z > 1 || projected.z < -1) return false;
        out[0] = (projected.x + 1) / 2 * rect.width;
        out[1] = (1 - projected.y) / 2 * rect.height;
        return true;
      });
    };
    let painting: { pointerId: number; value: boolean; last: number } | null = null;
    let hover: number | null = null;
    const setHover = (next: number | null) => {
      if (next === hover) return;
      hover = next;
      canvas.style.cursor = next === null ? '' : 'pointer';
      setHovered(next);
    };
    const tolerance = (event: PointerEvent) => PICK_TOLERANCE[event.pointerType as keyof typeof PICK_TOLERANCE] ?? 16;
    const pointerDown = (event: PointerEvent) => {
      const { isolation, onPaint } = current.current;
      if (!isolation.enabled || event.button !== 0 || event.shiftKey || event.ctrlKey || event.metaKey || painting) return;
      const hit = pick(event.clientX, event.clientY, tolerance(event));
      if (hit === null) { setHover(null); return; }
      event.stopImmediatePropagation();
      event.preventDefault();
      canvas.focus({ preventScroll: true });
      try { canvas.setPointerCapture(event.pointerId); } catch { /* capture is a nicety */ }
      painting = { pointerId: event.pointerId, value: !isolation.lit[hit], last: hit };
      canvas.classList.add('painting');
      onPaint(hit, hit, painting.value);
      setHover(hit);
    };
    const pointerMove = (event: PointerEvent) => {
      const { isolation, onPaint } = current.current;
      if (!isolation.enabled) { setHover(null); return; }
      if (painting) {
        if (event.pointerId !== painting.pointerId) return;
        event.stopImmediatePropagation();
        const hit = pick(event.clientX, event.clientY, tolerance(event) * 1.5);
        if (hit !== null && hit !== painting.last) {
          // Sweep every tube between the last hit and this one so fast drags never skip.
          onPaint(painting.last, hit, painting.value);
          painting.last = hit;
          setHover(hit);
        }
        return;
      }
      if (event.pointerType === 'touch' || event.buttons) return;
      setHover(pick(event.clientX, event.clientY, tolerance(event)));
    };
    const pointerEnd = (event: PointerEvent) => {
      if (!painting || event.pointerId !== painting.pointerId) return;
      event.stopImmediatePropagation();
      painting = null;
      canvas.classList.remove('painting');
      try { canvas.releasePointerCapture(event.pointerId); } catch { /* already released */ }
      current.current.onPaintEnd();
      if (event.pointerType === 'touch') setHover(null);
    };
    const pointerLeave = () => { if (!painting) setHover(null); };
    canvas.addEventListener('pointerdown', pointerDown, { capture: true });
    canvas.addEventListener('pointermove', pointerMove, { capture: true });
    canvas.addEventListener('pointerup', pointerEnd, { capture: true });
    canvas.addEventListener('pointercancel', pointerEnd, { capture: true });
    canvas.addEventListener('pointerleave', pointerLeave);

    const controls = attachViewControls(camera, canvas, view => { current.current.viewRef.current = view; });
    props.restoreViewRef.current = controls.restore;
    renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 2));
    renderer.setClearColor(0x000000, 0);
    let lines: LineMesh[] = [];
    let reflections: LineMesh[] = [];
    let shape = '';
    let palette = '';
    let revision = -1;
    let previousHistory: FrequencyHistory | null = null;
    // Samples arrive at the daemon's analysis rate (about 30 Hz) while the page
    // draws at the display rate. Between two samples the whole trace glides a
    // fraction of one column leftward and the newest column eases up to its
    // value, so motion stays continuous without inventing extra samples.
    let columnAt = Number.NEGATIVE_INFINITY;
    let columnInterval = 33;
    let lastSequence = -1;
    let frameId = 0;
    let stopped = false;
    const transition = new ThemeTransition(props.theme);
    const linearStart = new Float32Array(MAX_HISTORY * 3);
    const linearEnd = new Float32Array(MAX_HISTORY * 3);
    const startColor = new THREE.Color(), endColor = new THREE.Color();

    // --- Neon pipeline, built on first use and only run while isolation is on.
    const quadCamera = new THREE.OrthographicCamera(-1, 1, 1, -1, 0, 1);
    const quadScene = new THREE.Scene();
    const quad = new THREE.Mesh(new THREE.PlaneGeometry(2, 2));
    quad.frustumCulled = false;
    quadScene.add(quad);
    const brightMaterial = new THREE.ShaderMaterial({ vertexShader: QUAD_VERTEX, fragmentShader: BRIGHT_FRAGMENT, depthTest: false, depthWrite: false,
      uniforms: { tDiffuse: { value: null }, texel: { value: new THREE.Vector2() }, threshold: { value: BLOOM_THRESHOLD } } });
    const blurMaterial = new THREE.ShaderMaterial({ vertexShader: QUAD_VERTEX, fragmentShader: BLUR_FRAGMENT, depthTest: false, depthWrite: false,
      uniforms: { tDiffuse: { value: null }, step: { value: new THREE.Vector2() } } });
    const compositeMaterial = new THREE.ShaderMaterial({ vertexShader: QUAD_VERTEX, fragmentShader: COMPOSITE_FRAGMENT, depthTest: false, depthWrite: false,
      uniforms: { tScene: { value: null }, tBloom: { value: null }, strength: { value: BLOOM_STRENGTH },
        topColor: { value: new THREE.Color() }, bottomColor: { value: new THREE.Color() }, tintTop: { value: new THREE.Color() }, tintBottom: { value: new THREE.Color() } } });
    let targets: { key: string; scene: THREE.WebGLRenderTarget; a: THREE.WebGLRenderTarget; b: THREE.WebGLRenderTarget } | null = null;
    const disposeTargets = () => { targets?.scene.dispose(); targets?.a.dispose(); targets?.b.dispose(); targets = null; };
    const ensureTargets = () => {
      const size = renderer.getDrawingBufferSize(new THREE.Vector2());
      const key = `${size.x}x${size.y}`;
      if (targets?.key === key) return targets;
      disposeTargets();
      const webgl2 = renderer.capabilities.isWebGL2;
      const hdr = webgl2 ? renderer.extensions.has('EXT_color_buffer_float') : renderer.extensions.has('EXT_color_buffer_half_float');
      const type = hdr ? THREE.HalfFloatType : THREE.UnsignedByteType;
      const half = { x: Math.max(1, Math.floor(size.x / 2)), y: Math.max(1, Math.floor(size.y / 2)) };
      targets = {
        key,
        scene: new THREE.WebGLRenderTarget(size.x, size.y, { type, samples: webgl2 ? 4 : 0, depthBuffer: true }),
        a: new THREE.WebGLRenderTarget(half.x, half.y, { type, depthBuffer: false }),
        b: new THREE.WebGLRenderTarget(half.x, half.y, { type, depthBuffer: false }),
      };
      return targets;
    };
    const blit = (material: THREE.ShaderMaterial, target: THREE.WebGLRenderTarget | null) => {
      quad.material = material;
      renderer.setRenderTarget(target);
      renderer.render(quadScene, quadCamera);
    };
    const renderNeon = () => {
      const { scene: sceneTarget, a, b } = ensureTargets();
      renderer.setRenderTarget(sceneTarget);
      renderer.setClearColor(0x000000, 0);
      renderer.clear();
      renderer.render(scene, camera);
      brightMaterial.uniforms.tDiffuse.value = sceneTarget.texture;
      (brightMaterial.uniforms.texel.value as THREE.Vector2).set(1 / sceneTarget.width, 1 / sceneTarget.height);
      blit(brightMaterial, a);
      let read = a, write = b;
      for (const radius of BLOOM_STEPS) {
        blurMaterial.uniforms.tDiffuse.value = read.texture;
        (blurMaterial.uniforms.step.value as THREE.Vector2).set(radius / read.width, 0);
        blit(blurMaterial, write);
        [read, write] = [write, read];
        blurMaterial.uniforms.tDiffuse.value = read.texture;
        (blurMaterial.uniforms.step.value as THREE.Vector2).set(0, radius / read.height);
        blit(blurMaterial, write);
        [read, write] = [write, read];
      }
      compositeMaterial.uniforms.tScene.value = sceneTarget.texture;
      compositeMaterial.uniforms.tBloom.value = read.texture;
      blit(compositeMaterial, null);
    };
    let backgroundKey = '';
    const updateBackground = (shown: ThemePreset) => {
      const area = host.getBoundingClientRect();
      const app = host.parentElement?.getBoundingClientRect() ?? area;
      const key = `${themeKey(shown)}/${Math.round(area.top)}/${Math.round(area.bottom)}/${Math.round(app.top)}/${Math.round(app.height)}`;
      if (key === backgroundKey) return;
      backgroundKey = key;
      const position = (y: number) => Math.max(0, Math.min(1, (y - app.top) / Math.max(1, app.height)));
      const u = compositeMaterial.uniforms;
      (u.topColor.value as THREE.Color).set(mixColor(shown.uiTheme.background, '#000000', position(area.top)));
      (u.bottomColor.value as THREE.Color).set(mixColor(shown.uiTheme.background, '#000000', position(area.bottom)));
      (u.tintTop.value as THREE.Color).set(shown.graphTheme.line1);
      (u.tintBottom.value as THREE.Color).set(shown.graphTheme.line2);
      reflectionFog.color.copy(u.bottomColor.value as THREE.Color);
    };
    const disposeLines = () => {
      lines.forEach(line => { group.remove(line); line.geometry.dispose(); line.material.dispose(); });
      reflections.forEach(line => { mirror.remove(line); line.material.dispose(); });
      lines = [];
      reflections = [];
    };
    const resize = () => {
      const width = Math.max(1, host.clientWidth), height = Math.max(1, host.clientHeight);
      camera.aspect = width / height;
      camera.updateProjectionMatrix();
      renderer.setSize(width, height);
      backgroundKey = '';
    };
    const observer = new ResizeObserver(resize);
    observer.observe(host);
    resize();

    const render = () => {
      const { theme, lineSpacing, numBands, historyRef, isolation } = current.current;
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
          const material = new THREE.LineBasicMaterial({ vertexColors: true, opacity: 0.8 + i / numBands * 0.2, transparent: true, toneMapped: false, fog: false });
          const line = new THREE.Line(geometry, material);
          line.frustumCulled = false;
          lines.push(line);
          group.add(line);
          // The reflection shares the geometry, so it follows every sample and glide for free.
          const reflection = new THREE.Line(geometry, new THREE.LineBasicMaterial({ vertexColors: true, opacity: REFLECTION_OPACITY, transparent: true, toneMapped: false, depthWrite: false }));
          reflection.frustumCulled = false;
          reflections.push(reflection);
          mirror.add(reflection);
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
        if (previousHistory === history && history.sequence !== lastSequence && Number.isFinite(columnAt)) {
          // A scrolled-in column: measure the cadence and restart the glide.
          // A clear or band change leaves the sequence alone and snaps instead.
          const elapsed = now - columnAt;
          if (elapsed > 0 && elapsed < 250) columnInterval = columnInterval * 0.8 + elapsed * 0.2;
          columnAt = now;
        } else {
          columnAt = previousHistory === history && history.sequence !== lastSequence ? now : Number.NEGATIVE_INFINITY;
        }
        lastSequence = history.sequence;
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
      const phase = Math.min(1, Math.max(0, (now - columnAt) / Math.max(16, Math.min(100, columnInterval))));
      const eased = phase < 1 ? 1 - (1 - phase) * (1 - phase) : 1;
      // Each column is 0.05 units apart before the group's 15x stretch.
      group.position.x = (1 - phase) * 0.05 * group.scale.x;
      mirror.position.x = group.position.x;
      if (eased < 1 || group.userData.settling) {
        lines.forEach((line, i) => {
          const positions = line.geometry.getAttribute('position') as THREE.BufferAttribute;
          const row = history.samples[i];
          const previous = row?.[MAX_HISTORY - 2] ?? 0, latest = row?.[MAX_HISTORY - 1] ?? 0;
          positions.setY(MAX_HISTORY - 1, (previous + (latest - previous) * eased) * 30);
          positions.needsUpdate = true;
        });
        group.userData.settling = eased < 1;
      }

      // --- Tube state: plain traces, or neon lit tubes over dark tinted ones.
      const neon = isolation.enabled;
      const anyLit = neon && isolation.lit.some(Boolean);
      mirror.visible = neon;
      scene.fog = neon ? reflectionFog : null;
      lines.forEach((line, i) => {
        const lit = !anyLit || isolation.lit[i] === true;
        const isHover = hover === i;
        const level = !neon ? 1 : lit ? (isHover ? LIT_HOVER : LIT) : (isHover ? DARK_HOVER : DARK);
        line.material.color.setScalar(level);
        line.material.opacity = !neon ? 0.8 + i / numBands * 0.2 : lit ? 1 : 0.85;
        const reflection = reflections[i];
        if (reflection) {
          reflection.visible = neon && lit;
          reflection.material.color.setScalar(REFLECTION);
        }
      });
      controls.update();
      if (neon) {
        updateBackground(displayed);
        renderNeon();
      } else {
        renderer.setRenderTarget(null);
        renderer.render(scene, camera);
      }
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
      paintBackdrop(context, canvas.width, canvas.height, shown, area, app);
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
      canvas.removeEventListener('pointerdown', pointerDown, { capture: true });
      canvas.removeEventListener('pointermove', pointerMove, { capture: true });
      canvas.removeEventListener('pointerup', pointerEnd, { capture: true });
      canvas.removeEventListener('pointercancel', pointerEnd, { capture: true });
      canvas.removeEventListener('pointerleave', pointerLeave);
      current.current.restoreViewRef.current = null;
      renderer.domElement.removeEventListener('webglcontextlost', lost);
      if (current.current.captureRef.current === capture) current.current.captureRef.current = null;
      disposeLines();
      disposeTargets();
      quad.geometry.dispose();
      brightMaterial.dispose(); blurMaterial.dispose(); compositeMaterial.dispose();
      scene.clear();
      renderer.dispose();
      renderer.forceContextLoss();
      renderer.domElement.remove();
    };
  }, []);

  const { isolation, numBands, historyRef } = props;
  const litCount = isolation.lit.filter(Boolean).length;
  return <div ref={container} className={`visualizer${isolation.enabled ? ' is-isolating' : ''}`}>
    {!error && !isolation.enabled && <p className="gesture-hint"><span className="mouse-hint">Drag to rotate · WASD to move · Q/E down/up · Scroll to zoom · Double-click to reset</span><span className="touch-hint">Drag to rotate · Two fingers to move and zoom</span></p>}
    {!error && isolation.enabled && <p className="gesture-hint isolation-hint" role="status">
      {hovered !== null ? describeBand(hovered, numBands, historyRef.current.sampleRate)
        : litCount === 0 ? 'Click a tube to light it · drag across tubes to paint · drag the backdrop to rotate'
          : `${litCount} of ${numBands} tubes lit · only those play`}
    </p>}
    {error && <p className="canvas-error" role="alert">{error}</p>}
  </div>;
}
