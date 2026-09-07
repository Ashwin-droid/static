import { presets, type ThemePreset } from './themes';

const record = (value: unknown): value is Record<string, unknown> => typeof value === 'object' && value !== null;
const rgb = (value: unknown): value is string => typeof value === 'string' && value.length === 7 && /^#[\da-f]{6}$/i.test(value);

/** Only retain the five RGB colours Rust generated, never arbitrary style values. */
export function parseCoverTheme(value: unknown): ThemePreset | null {
  if (!record(value) || value.name !== 'Cover art') return null;
  const ui = value.uiTheme;
  const graph = value.graphTheme;
  if (!record(ui) || !record(graph) || !rgb(ui.primary) || !rgb(ui.secondary) ||
      !rgb(ui.background) || !rgb(graph.line1) || !rgb(graph.line2)) return null;
  return { name: 'Cover art', uiTheme: { primary: ui.primary, secondary: ui.secondary, background: ui.background },
    graphTheme: { line1: graph.line1, line2: graph.line2 } };
}

export interface ThemeSelection {
  videoId: string | null;
  mode: 'cover' | 'manual';
  cover: ThemePreset | null;
  manual: ThemePreset;
}
export type ThemeAction =
  | { type: 'track'; videoId: string | null }
  | { type: 'cover'; videoId: string | null; theme: unknown }
  | { type: 'automatic' }
  | { type: 'manual'; theme: ThemePreset };

export const initialThemeSelection = (): ThemeSelection => ({ videoId: null, mode: 'cover', cover: null, manual: presets[0] });
export const activeTheme = (state: ThemeSelection): ThemePreset => state.mode === 'cover' ? state.cover ?? presets[0] : state.manual;

export function selectTheme(state: ThemeSelection, action: ThemeAction): ThemeSelection {
  switch (action.type) {
    case 'track':
      return action.videoId === state.videoId ? state : { ...state, videoId: action.videoId, cover: null };
    case 'cover': {
      if (action.videoId !== state.videoId) return state;
      if (action.theme === null) return state.cover === null ? state : { ...state, cover: null };
      if (action.videoId === null) return state;
      const cover = parseCoverTheme(action.theme);
      return cover ? { ...state, cover } : state;
    }
    case 'automatic': return state.mode === 'cover' ? state : { ...state, mode: 'cover' };
    case 'manual': return { ...state, mode: 'manual', manual: action.theme };
  }
}
