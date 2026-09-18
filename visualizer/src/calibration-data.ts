export interface GainSettings {
  enabled: boolean; flatten_percent: number; ra_seconds: number; change_lazyness_percent: number; xdgain_enabled: boolean;
}
export interface GainFrame {
  generation: number; sequence: number; position_millis: number | null; opening_guard_millis:number; settings: GainSettings;
  input_rms: number; input_peak: number; gain_output_rms: number; gain_output_peak: number; output_rms: number; output_peak: number; rolling_rms: number;
  window_seconds: number; audible_seconds: number; requested_gain: number; preview_gain: number;
  applied_gain: number; xdgain_reference_dbfs: number | null; limiter_gain: number; master_gain: number;
}
export interface Diagnostics { type: 'diagnostics'; frame: GainFrame; paused: boolean; track: {video_id:string; title:string; artist:string} | null }
export const defaultGainSettings: GainSettings = { enabled:false, flatten_percent:50, ra_seconds:15, change_lazyness_percent:50, xdgain_enabled:false };
export const db = (amplitude: number) => Math.max(-96, 20 * Math.log10(Math.max(0.000001, amplitude)));
export const level = (amplitude: number | null | undefined) => amplitude == null || amplitude <= 0 ? '−∞' : db(amplitude).toFixed(1);
export const gainDb = (ratio: number | undefined) => ratio == null ? '—' : ratio <= 0 ? '−∞' : `${db(ratio) >= 0 ? '+' : ''}${db(ratio).toFixed(1)}`;
const record = (value: unknown): value is Record<string, any> => value !== null && typeof value === 'object';
const finite = (value: unknown): value is number => typeof value === 'number' && Number.isFinite(value);
const within = (value: unknown, min:number, max:number) => finite(value) && value >= min && value <= max;
export function parseDiagnostics(raw: unknown): Diagnostics | null {
  if (!record(raw) || raw.type !== 'diagnostics' || typeof raw.paused !== 'boolean' || !record(raw.frame)) return null;
  const f = raw.frame, s = f.settings;
  if (!record(s) || typeof s.enabled !== 'boolean' || typeof s.xdgain_enabled !== 'boolean' ||
      !within(s.flatten_percent,0,100) || !within(s.ra_seconds,0.1,120) || !within(s.change_lazyness_percent,0,100)) return null;
  if (!Number.isSafeInteger(f.generation) || f.generation < 0 || !Number.isSafeInteger(f.sequence) || f.sequence < 0 ||
      !(f.position_millis === null || (Number.isSafeInteger(f.position_millis) && f.position_millis >= 0)) ||
      !within(f.opening_guard_millis,0,15000) ||
      !['input_rms','input_peak','gain_output_rms','gain_output_peak','output_rms','output_peak','rolling_rms','window_seconds','audible_seconds','requested_gain','preview_gain','applied_gain','limiter_gain','master_gain'].every(key => finite(f[key]) && f[key] >= 0) ||
      !(f.xdgain_reference_dbfs === null || finite(f.xdgain_reference_dbfs))) return null;
  if (!(raw.track === null || (record(raw.track) && ['video_id','title','artist'].every(key => typeof raw.track[key] === 'string')))) return null;
  return raw as unknown as Diagnostics;
}
export type Point = { time:number; frame:GainFrame };
/** A minute of live measurements. Fresh generations never share an average. */
export class GainHistory {
  readonly points: Point[] = [];
  generation = -1; sequence = -1;
  clear() { this.points.length = 0; this.generation = -1; this.sequence = -1; }
  append(data: Diagnostics, now: number) {
    if (data.paused || !data.track || data.frame.sequence === 0) return false;
    const frame = data.frame;
    if (frame.generation < this.generation || (frame.generation === this.generation && frame.sequence <= this.sequence)) return false;
    if (frame.generation !== this.generation) this.clear();
    this.generation = frame.generation; this.sequence = frame.sequence;
    this.points.push({ time: now, frame });
    while (this.points.length > 600 || this.points[0]?.time < now-60_000) this.points.shift();
    return true;
  }
}

/** Fractions at one RA interval; the previous-song term reserves exactly 20%. */
export function wraWeights(settings: GainSettings, hasReference: boolean) {
  const history = 0.1 + 0.8 * settings.change_lazyness_percent / 100;
  const reference = settings.xdgain_enabled && hasReference ? 0.2 : 0;
  return { input: (1-reference)*(1-history), reference, history: (1-reference)*history };
}
