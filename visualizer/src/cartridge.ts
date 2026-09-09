import type { CameraControls } from './view-controls';
import { parseCoverTheme } from './theme-state';
import type { ThemePreset } from './themes';

export interface Cartridge {
  videoId: string;
  theme: ThemePreset;
  camera: CameraControls;
  spacing: number;
  bands: number;
}

/** Treat even recovered metadata as input; only these values can reach Three.js. */
export function parseCartridge(value: unknown): Cartridge | null {
  if (!value || typeof value !== 'object') return null;
  const m = value as Record<string, any>;
  const theme = parseCoverTheme({ name: 'Cover art', uiTheme: m.uiTheme, graphTheme: m.graphTheme });
  const inRange = (v: unknown, min: number, max: number): v is number => typeof v === 'number' && Number.isFinite(v) && v >= min && v <= max;
  const c = m.cameraControls;
  if (!theme || typeof m.videoId !== 'string' || !/^[\w-]{11}$/.test(m.videoId) || !c?.position || !c?.rotation ||
      !['x', 'y', 'z'].every(axis => inRange(c.position[axis], -256, 254)) ||
      !inRange(c.rotation.yaw, 0, Math.PI * 2) || !inRange(c.rotation.roll, 0, Math.PI) || c.rotation.pitch !== 0 ||
      !inRange(c.distance, 5, 255) || !inRange(m.lineSpacing, 0.05, 12.75) ||
      !Number.isInteger(m.fftSize) || !inRange(m.fftSize, 8, 128)) return null;
  return { videoId: m.videoId, theme: { ...theme, name: '' }, camera: {
    position: { x: c.position.x, y: c.position.y, z: c.position.z },
    rotation: { yaw: c.rotation.yaw, roll: c.rotation.roll, pitch: 0 }, distance: c.distance,
  }, spacing: m.lineSpacing, bands: m.fftSize };
}

export async function readCartridge(file: File, origin: string, hash: string, signal: AbortSignal): Promise<Cartridge> {
  if (file.size > 16 * 1024 * 1024) throw new Error('Cartridge images must be smaller than 16 MiB.');
  const token = new URLSearchParams(hash.replace(/^#/, '')).get('token');
  if (!token) throw new Error('Open the visualiser from Static to read a cartridge.');
  const url = new URL('/cartridge/decode', origin);
  url.searchParams.set('token', token);
  // Stream the selected file to Rust. No browser image decode, pixel work or base64.
  const reply = await fetch(url, { method: 'POST', body: file, signal });
  if (!reply.ok) throw new Error(await reply.text() || 'Could not read this cartridge.');
  const cartridge = parseCartridge(await reply.json());
  if (!cartridge) throw new Error('Static returned invalid cartridge settings.');
  return cartridge;
}
