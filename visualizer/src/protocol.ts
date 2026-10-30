import { parseCoverTheme } from './theme-state';
import { presets, type ThemePreset } from './themes';

export const MAX_HISTORY = 350;
export const MIN_BANDS = 8;
export const MAX_BANDS = 128;

export interface PlaybackSnapshot {
  current_track: { video_id: string; title: string; artist: string } | null;
  paused: boolean;
  volume: number;
  max_volume: number;
  position_millis: number | null;
  duration_millis: number | null;
  /** Bands the daemon is currently isolating; null plays everything. */
  band_isolation: BandIsolation | null;
}
export interface BandIsolation { bands: number; selected: number[] }
export interface AudioFrame {
  generation: number;
  sequence: number;
  sample_rate: number;
  bands: number[];
  rms: number;
  peak: number;
}
export type ServerMessage =
  | { type: 'state'; state: PlaybackSnapshot }
  | { type: 'theme'; video_id: string | null; theme: ThemePreset | null }
  | { type: 'audio'; frame: AudioFrame }
  | { type: 'error'; message: string; id?: string }
  | { type: 'cartridge_ready'; id: string; filename: string; url: string; storage: 'memory' | 'disk'; path: string | null };

const record = (v: unknown): v is Record<string, unknown> => typeof v === 'object' && v !== null;
const finite = (v: unknown): v is number => typeof v === 'number' && Number.isFinite(v);
const uint = (v: unknown): v is number => finite(v) && Number.isSafeInteger(v) && v >= 0;
const string = (v: unknown): v is string => typeof v === 'string';

/** Keep invalid or unexpectedly large messages away from React and the GPU. */
export function parseMessage(raw: unknown): ServerMessage | null {
  if (typeof raw !== 'string' || raw.length > 2_000_000) return null;
  try {
    const m: unknown = JSON.parse(raw);
    if (!record(m)) return null;
    switch (m.type) {
      case 'theme': {
        if (!(m.video_id === null || string(m.video_id))) return null;
        if (m.theme === null) return { type: 'theme', video_id: m.video_id, theme: null };
        const theme = parseCoverTheme(m.theme);
        return theme && m.video_id !== null ? { type: 'theme', video_id: m.video_id, theme } : null;
      }
      case 'audio': {
        const f = m.frame;
        if (!record(f) || !uint(f.generation) || !uint(f.sequence) || !finite(f.sample_rate) ||
            f.sample_rate <= 0 || !finite(f.rms) || !finite(f.peak) || !Array.isArray(f.bands) ||
            f.bands.length < MIN_BANDS || f.bands.length > MAX_BANDS ||
            !f.bands.every(v => finite(v) && v >= 0 && v <= 1)) return null;
        return m as unknown as ServerMessage;
      }
      case 'state': {
        const s = m.state;
        if (!record(s) || typeof s.paused !== 'boolean' || !finite(s.volume) ||
            !finite(s.max_volume) || s.max_volume <= 0 || s.max_volume > 5 ||
            s.volume < 0 || s.volume > 5 ||
            !(s.position_millis === null || uint(s.position_millis)) ||
            !(s.duration_millis === null || uint(s.duration_millis))) return null;
        const t = s.current_track;
        if (!(t === null || (record(t) && string(t.video_id) && string(t.title) && string(t.artist)))) return null;
        const b = s.band_isolation;
        let band_isolation: BandIsolation | null = null;
        if (b !== undefined && b !== null) {
          if (!record(b) || !uint(b.bands) || b.bands < MIN_BANDS || b.bands > MAX_BANDS || !Array.isArray(b.selected) ||
              b.selected.length > b.bands || !b.selected.every(v => uint(v) && v < (b.bands as number))) return null;
          band_isolation = { bands: b.bands, selected: b.selected as number[] };
        }
        // Avoid retaining the daemon queue, artwork URLs or other unrelated snapshot data.
        return { type: 'state', state: {
          current_track: t === null ? null : { video_id: t.video_id as string, title: t.title as string, artist: t.artist as string },
          paused: s.paused, volume: s.volume, max_volume: s.max_volume,
          position_millis: s.position_millis as number | null,
          duration_millis: s.duration_millis as number | null,
          band_isolation,
        } };
      }
      case 'error':
        return string(m.message) && (m.id === undefined || string(m.id)) ? m as ServerMessage : null;
      case 'cartridge_ready':
        return ['id', 'filename', 'url'].every(k => string(m[k]))
          && ((m.storage === 'memory' && (m.path === null || string(m.path)))
            || (m.storage === 'disk' && string(m.path))) ? m as ServerMessage : null;
      default: return null;
    }
  } catch { return null; }
}

/** A fixed allocation: at most 128 bands × 350 float samples. */
export class FrequencyHistory {
  readonly samples: Float32Array[];
  /** Immutable-at-birth palette endpoints for each time column, packed as RGB. */
  readonly line1Colors = new Uint32Array(MAX_HISTORY);
  readonly line2Colors = new Uint32Array(MAX_HISTORY);
  private lastLine1: number;
  private lastLine2: number;
  generation: number | null = null;
  sequence = -1;
  revision = 0;
  /** From the latest frame; band edges in Hz depend on it. */
  sampleRate = 48_000;
  constructor(readonly bands: number, colors = presets[0].graphTheme) {
    if (!Number.isInteger(bands) || bands < MIN_BANDS || bands > MAX_BANDS) throw new RangeError('Bands must be 8 to 128');
    this.samples = Array.from({ length: bands }, () => new Float32Array(MAX_HISTORY));
    this.lastLine1 = Number.parseInt(colors.line1.slice(1), 16);
    this.lastLine2 = Number.parseInt(colors.line2.slice(1), 16);
    this.line1Colors.fill(this.lastLine1);
    this.line2Colors.fill(this.lastLine2);
  }
  clear(colors?: ThemePreset['graphTheme']) {
    if (colors) {
      this.lastLine1 = Number.parseInt(colors.line1.slice(1), 16);
      this.lastLine2 = Number.parseInt(colors.line2.slice(1), 16);
    }
    this.samples.forEach(row => row.fill(0));
    this.line1Colors.fill(this.lastLine1);
    this.line2Colors.fill(this.lastLine2);
    this.revision++;
  }
  /** A reconnected producer may restart its counters; visible history remains. */
  resetStream() {
    this.generation = null;
    this.sequence = -1;
  }
  append(frame: AudioFrame, paused: boolean, colors?: ThemePreset['graphTheme']): boolean {
    if (this.generation !== null && frame.generation < this.generation) return false;
    if (frame.generation !== this.generation) {
      this.generation = frame.generation;
      this.sequence = -1;
    }
    if (frame.sequence <= this.sequence || frame.bands.length !== this.bands) return false;
    this.sequence = frame.sequence;
    this.sampleRate = frame.sample_rate;
    if (paused) return false;
    if (colors) {
      this.lastLine1 = Number.parseInt(colors.line1.slice(1), 16);
      this.lastLine2 = Number.parseInt(colors.line2.slice(1), 16);
    }
    this.line1Colors.copyWithin(0, 1);
    this.line2Colors.copyWithin(0, 1);
    this.line1Colors[MAX_HISTORY - 1] = this.lastLine1;
    this.line2Colors[MAX_HISTORY - 1] = this.lastLine2;
    this.samples.forEach((row, i) => {
      row.copyWithin(0, 1);
      row[MAX_HISTORY - 1] = frame.bands[i];
    });
    this.revision++;
    return true;
  }
}

export function socketUrl(host: string, hash: string): string | null {
  const token = new URLSearchParams(hash.replace(/^#/, '')).get('token');
  return token ? `wss://${host}/ws?token=${encodeURIComponent(token)}` : null;
}

export function downloadUrl(value: string, origin: string): string | null {
  try {
    const url = new URL(value, origin);
    return url.origin === origin && url.protocol === 'https:' && url.pathname.startsWith('/cartridges/') ? url.href : null;
  } catch { return null; }
}

export const reconnectDelay = (attempt: number) => Math.min(15_000, 500 * 2 ** Math.min(5, attempt));
