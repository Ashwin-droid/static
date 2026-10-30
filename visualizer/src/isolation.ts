/** Band isolation: which tubes are lit, and the band math shared with the daemon. */

export interface Isolation {
  /** Off by default: lighting tubes is opt-in so ordinary drags never change playback. */
  enabled: boolean;
  /** One flag per band; all false means everything plays. */
  lit: boolean[];
}

export const isolationOff = (bands: number): Isolation => ({ enabled: false, lit: new Array<boolean>(bands).fill(false) });

export const litIndices = (lit: boolean[]) => lit.flatMap((on, i) => on ? [i] : []);
export const anyLit = (lit: boolean[]) => lit.some(Boolean);

/** Inclusive range paint; `from`/`to` may arrive in either order. */
export function paintRange(lit: boolean[], from: number, to: number, value: boolean): boolean[] {
  const next = lit.slice();
  const [a, b] = from <= to ? [from, to] : [to, from];
  for (let i = Math.max(0, a); i <= Math.min(next.length - 1, b); i++) next[i] = value;
  return next;
}

/** Changing the band count keeps the lit *frequencies*: a new band is lit when its centre fell in a lit old band. */
export function remapLit(lit: boolean[], bands: number): boolean[] {
  const old = lit.length;
  if (old === bands) return lit.slice();
  if (old === 0) return new Array<boolean>(bands).fill(false);
  return Array.from({ length: bands }, (_, j) => lit[Math.min(old - 1, Math.floor((j + 0.5) / bands * old))] ?? false);
}

/** Log-spaced edges from 20 Hz to Nyquist, matching the daemon's analyser. */
export function bandHz(index: number, bands: number, sampleRate: number): [number, number] {
  const nyquist = sampleRate / 2;
  const low = Math.min(20, nyquist / 2);
  const ratio = nyquist / low;
  return [low * ratio ** (index / bands), low * ratio ** ((index + 1) / bands)];
}

export function formatHz(hz: number): string {
  return hz >= 1000 ? `${(hz / 1000).toFixed(hz >= 10_000 ? 0 : 1)} kHz` : `${Math.round(hz)} Hz`;
}

export const describeBand = (index: number, bands: number, sampleRate: number) => {
  const [start, end] = bandHz(index, bands, sampleRate);
  return `Band ${index + 1} of ${bands} · ${formatHz(start)} – ${formatHz(end)}`;
};

/** Wire form of a selection for the daemon; `null` when nothing is lit. */
export const isolationKey = (bands: number, lit: boolean[]) => anyLit(lit) ? `${bands}:${litIndices(lit).join(',')}` : '';

/** Squared distance from a point to a segment, all in pixels. */
export function segmentDistanceSq(px: number, py: number, ax: number, ay: number, bx: number, by: number): number {
  const dx = bx - ax, dy = by - ay;
  const lengthSq = dx * dx + dy * dy;
  const t = lengthSq === 0 ? 0 : Math.max(0, Math.min(1, ((px - ax) * dx + (py - ay) * dy) / lengthSq));
  const cx = ax + dx * t - px, cy = ay + dy * t - py;
  return cx * cx + cy * cy;
}

/**
 * Nearest polyline to a pointer, in screen space, so a pick is exactly what the
 * eye sees regardless of perspective. `screen(band, column)` fills `out` with
 * pixel x/y and returns false when that vertex is behind the camera.
 */
export function pickPolyline(bands: number, columns: number, px: number, py: number, tolerance: number,
  screen: (band: number, column: number, out: [number, number]) => boolean, step = 2): number | null {
  let best = tolerance * tolerance, found: number | null = null;
  const a: [number, number] = [0, 0], b: [number, number] = [0, 0];
  for (let band = 0; band < bands; band++) {
    let previous = screen(band, 0, a);
    for (let column = step; column < columns + step; column += step) {
      const index = Math.min(columns - 1, column);
      const visible = screen(band, index, b);
      if (previous && visible) {
        const distance = segmentDistanceSq(px, py, a[0], a[1], b[0], b[1]);
        if (distance < best) { best = distance; found = band; }
      }
      a[0] = b[0]; a[1] = b[1]; previous = visible;
      if (index === columns - 1) break;
    }
  }
  return found;
}
