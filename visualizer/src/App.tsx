import { useEffect, useRef, useState } from 'react';
import { HardDrive, Music, Pause, Play, RotateCcw, SkipBack, SkipForward, Sliders, Volume2, Maximize, Minimize } from 'lucide-react';
import { Visualizer } from './Visualizer';
import { initialCamera, type CameraControls } from './view-controls';
import { readCartridge, type Cartridge } from './cartridge';
import { FullscreenView, type FullscreenState } from './fullscreen';
import { connectStatic, type ConnectionStatus } from './connection';
import { downloadUrl, FrequencyHistory, socketUrl, type PlaybackSnapshot, type ServerMessage } from './protocol';
import { presets, type ThemePreset } from './themes';
import { activeTheme, initialThemeSelection, selectTheme, type ThemeAction } from './theme-state';

const initialState: PlaybackSnapshot = {
  current_track: null, paused: true, volume: 1, max_volume: 1, position_millis: null, duration_millis: null,
};
const tabs = [
  { id: 'audio', label: 'Audio', icon: Music },
  { id: 'config', label: 'Config', icon: Sliders },
  { id: 'cartridge', label: 'Cartridge', icon: HardDrive },
] as const;
type Tab = typeof tabs[number]['id'];
const time = (ms: number | null) => ms === null ? '--:--' : `${Math.floor(ms / 60_000)}:${String(Math.floor(ms / 1000) % 60).padStart(2, '0')}`;

export function App() {
  const host = useRef<HTMLElement>(null);
  const fullscreenController = useRef<FullscreenView | null>(null);
  const [fullscreen, setFullscreen] = useState<FullscreenState>({ active: false, controls: true });
  const [state, setState] = useState(initialState);
  const stateRef = useRef(state);
  const [status, setStatus] = useState<ConnectionStatus>('connecting');
  const [tab, setTab] = useState<Tab>('audio');
  const [bands, setBands] = useState(30);
  const bandsRef = useRef(bands);
  const [spacing, setSpacing] = useState(2);
  const view = useRef(initialCamera());
  const restoreView = useRef<((view: CameraControls) => void) | null>(null);
  const [imported, setImported] = useState<Cartridge | null>(null);
  const [reading, setReading] = useState(false);
  const importRequest = useRef<AbortController | null>(null);
  const [themeSelection, setThemeSelection] = useState(initialThemeSelection);
  const themeSelectionRef = useRef(themeSelection);
  const theme = activeTheme(themeSelection);
  const displayedTheme = useRef<ThemePreset>(theme);
  const paintTheme = (next: ThemePreset) => {
    const element = host.current;
    if (!element) return;
    element.style.setProperty('--primary', next.uiTheme.primary);
    element.style.setProperty('--secondary', next.uiTheme.secondary);
    element.style.setProperty('--visualizer-background', next.uiTheme.background);
    element.style.setProperty('--graph-start', next.graphTheme.line1);
    element.style.setProperty('--graph-end', next.graphTheme.line2);
  };
  const changeTheme = (action: ThemeAction) => {
    const next = selectTheme(themeSelectionRef.current, action);
    if (next === themeSelectionRef.current) return;
    themeSelectionRef.current = next;
    setThemeSelection(next);
  };
  const history = useRef(new FrequencyHistory(30));
  const capture = useRef<(() => string) | null>(null);
  const connection = useRef<ReturnType<typeof connectStatic> | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const request = useRef<string | null>(null);
  const requestTimeout = useRef<ReturnType<typeof setTimeout>>();
  const [cartridge, setCartridge] = useState<Extract<ServerMessage, { type: 'cartridge_ready' }> | null>(null);
  const lastVolume = useRef(1);

  useEffect(() => {
    if (!host.current) return;
    const controller = new FullscreenView(host.current, setFullscreen, setNotice);
    fullscreenController.current = controller;
    return () => { controller.dispose(); fullscreenController.current = null; };
  }, []);

  useEffect(() => {
    const clearRequest = () => {
      request.current = null;
      if (requestTimeout.current !== undefined) clearTimeout(requestTimeout.current);
      requestTimeout.current = undefined;
      setBusy(false);
    };
    connection.current = connectStatic({
      url: socketUrl(location.host, location.hash),
      bands: () => bandsRef.current,
      onStatus: value => {
        setStatus(value);
        if (value === 'connected') history.current.resetStream();
        if ((value === 'disconnected' || value === 'unavailable') && request.current) {
          clearRequest();
          setNotice('Connection lost during cartridge creation. A completed cartridge remains with the playback daemon; check staticfs if enabled.');
        }
      },
      onMessage: message => {
        switch (message.type) {
          case 'audio': history.current.append(message.frame, stateRef.current.paused || !stateRef.current.current_track, displayedTheme.current.graphTheme); break;
          case 'state':
            stateRef.current = message.state;
            setState(message.state);
            changeTheme({ type: 'track', videoId: message.state.current_track?.video_id ?? null });
            break;
          case 'theme': changeTheme({ type: 'cover', videoId: message.video_id, theme: message.theme }); break;
          case 'error':
            setNotice(message.message);
            if (message.id === request.current) clearRequest();
            break;
          case 'cartridge_ready':
            if (message.id !== request.current) break;
            clearRequest();
            if (downloadUrl(message.url, location.origin)) setCartridge(message);
            else setNotice('Static returned an invalid cartridge download address.');
            break;
        }
      },
    });
    return () => {
      importRequest.current?.abort();
      connection.current?.dispose();
      connection.current = null;
      if (requestTimeout.current !== undefined) clearTimeout(requestTimeout.current);
    };
  }, []);

  const send = (message: unknown) => {
    const sent = connection.current?.send(message) ?? false;
    if (!sent) setNotice('Static is disconnected. Reconnect before sending a command.');
    return sent;
  };
  const changeBands = (value: number) => {
    const next = Math.max(8, Math.min(128, Math.round(value)));
    bandsRef.current = next;
    setBands(next);
    history.current = new FrequencyHistory(next, displayedTheme.current.graphTheme);
    if (status === 'connected') send({ type: 'configure', bands: next });
  };
  const changeVolume = (value: number) => {
    const safe = Math.max(0, Math.min(state.max_volume, value));
    if (safe > 0) lastVolume.current = safe;
    send({ type: 'volume', value: safe });
  };
  const importCartridge = async (file: File) => {
    importRequest.current?.abort();
    const controller = new AbortController();
    importRequest.current = controller;
    setReading(true); setImported(null); setNotice(null);
    const timer = setTimeout(() => controller.abort(), 60_000);
    try {
      const recovered = await readCartridge(file, location.origin, location.hash, controller.signal);
      if (!controller.signal.aborted) setImported(recovered);
    } catch (error) {
      if (importRequest.current === controller) setNotice(controller.signal.aborted ? 'Reading the cartridge timed out.' : error instanceof Error ? error.message : 'Could not read the cartridge.');
    } finally {
      clearTimeout(timer);
      if (importRequest.current === controller) { setReading(false); importRequest.current = null; }
    }
  };
  const loadCartridge = () => {
    if (!imported || !send({ type: 'play_cartridge', video_id: imported.videoId })) return;
    changeTheme({ type: 'manual', theme: imported.theme });
    changeBands(imported.bands);
    setSpacing(imported.spacing);
    restoreView.current?.(imported.camera);
    setNotice('Cartridge view restored. Static is opening the song.');
  };
  const createCartridge = () => {
    if (!state.current_track || !capture.current || busy) return;
    setNotice(null);
    setCartridge(null);
    try {
      const screenshot = capture.current();
      const capturedTheme = displayedTheme.current;
      // A 1280 x 720 PNG normally stays well below this limit. Refuse noisy oversized captures.
      if (screenshot.length > 3_500_000) throw new Error('The screenshot is too large. Use a smaller window and try again.');
      const id = crypto.randomUUID();
      if (!send({ type: 'cartridge', id, video_id: state.current_track.video_id, screenshot,
        metadata: { uiTheme: capturedTheme.uiTheme, graphTheme: capturedTheme.graphTheme, cameraControls: view.current,
          lineSpacing: spacing, fftSize: bands, timestamp: Date.now() } })) return;
      request.current = id;
      setBusy(true);
      requestTimeout.current = setTimeout(() => {
        request.current = null;
        setBusy(false);
        setNotice('Cartridge creation has not responded. Check Static before trying again.');
      }, 300_000);
    } catch (error) { setNotice(error instanceof Error ? error.message : 'Could not capture the visualiser.'); }
  };
  const connected = status === 'connected';
  const color = (group: 'uiTheme' | 'graphTheme', key: string, value: string) => {
    if (!/^#[a-f\d]{6}$/i.test(value)) return;
    const previous = activeTheme(themeSelectionRef.current);
    changeTheme({ type: 'manual', theme: { ...previous, name: '', [group]: { ...previous[group], [key]: value } } as ThemePreset });
  };
  const showControls = !fullscreen.active || fullscreen.controls;
  return <main ref={host} className={`app${fullscreen.active ? ' is-fullscreen' : ''}`} data-controls={showControls}>
    <header className="topline" hidden={!showControls}><span>STATIC <span className="muted">/ echo trace</span></span><div className="window-actions"><span className="connection" data-connected={connected}>{connected ? 'Connected to Static' : 'Disconnected'}</span><button className="fullscreen-button" onClick={() => void fullscreenController.current?.toggle()} aria-label={fullscreen.active ? 'Exit fullscreen' : 'Enter fullscreen'} title={fullscreen.active ? 'Exit fullscreen (Escape / F11)' : 'Fullscreen (F11)'}>{fullscreen.active ? <Minimize size={16} /> : <Maximize size={16} />}</button></div></header>
    {fullscreen.active && <button className="controls-reveal" title={showControls ? 'Hide controls' : 'Show controls'} aria-label={showControls ? 'Hide controls' : 'Show controls'} aria-expanded={showControls} aria-controls="visualizer-controls" onClick={() => fullscreenController.current?.toggleControls()}><Sliders size={16} /></button>}
    {!connected && <div className="connection-banner" role="status" hidden={!showControls}>
      {status === 'connecting' ? 'Connecting to Static…' : status === 'disconnected' ? 'Connection lost. Reconnecting…' : 'Open the visualiser from Static to connect.'}
      {status === 'unavailable' && <button onClick={() => location.reload()}>Retry</button>}
    </div>}
    <Visualizer historyRef={history} captureRef={capture} viewRef={view} restoreViewRef={restoreView} theme={theme}
      displayedThemeRef={displayedTheme} onThemeFrame={paintTheme} lineSpacing={spacing} numBands={bands} />
    <section id="visualizer-controls" className="controls" aria-label="Visualiser controls" hidden={!showControls}>
      <div className="panel">
        <div className="tabs" role="tablist" aria-label="Controls">
          {tabs.map(({ id, label, icon: Icon }, i) => <button key={id} id={`tab-${id}`} role="tab" aria-selected={tab === id}
            aria-controls={`panel-${id}`} tabIndex={tab === id ? 0 : -1} onClick={() => setTab(id)}
            onKeyDown={event => {
              const next = event.key === 'ArrowRight' ? (i + 1) % tabs.length : event.key === 'ArrowLeft' ? (i + tabs.length - 1) % tabs.length : event.key === 'Home' ? 0 : event.key === 'End' ? tabs.length - 1 : null;
              if (next !== null) { event.preventDefault(); setTab(tabs[next].id); document.getElementById(`tab-${tabs[next].id}`)?.focus(); }
            }}><Icon size={15} />{label}</button>)}
        </div>
        <div className="panel-body" role="tabpanel" id={`panel-${tab}`} aria-labelledby={`tab-${tab}`}>
          {tab === 'audio' && <div className="audio-controls">
            <div className="track"><span className="track-icon"><Music size={20} /></span><div>
              <strong title={state.current_track?.title}>{state.current_track?.title ?? 'No song playing'}</strong>
              <p>{state.current_track?.artist || 'Choose a song in Static'}<span className="track-time">{time(state.position_millis)} / {time(state.duration_millis)}</span></p>
            </div></div>
            <div className="transport">
              <button className="icon-button" aria-label="Previous song" title="Previous song" disabled={!connected || !state.current_track} onClick={() => send({ type: 'transport', action: 'previous' })}><SkipBack size={16} /></button>
              <button className="icon-button play" aria-label={state.paused ? 'Play' : 'Pause'} title={state.paused ? 'Play' : 'Pause'} disabled={!connected || !state.current_track} onClick={() => send({ type: 'transport', action: 'toggle' })}>{state.paused ? <Play size={17} /> : <Pause size={17} />}</button>
              <button className="icon-button" aria-label="Next song" title="Next song" disabled={!connected || !state.current_track} onClick={() => send({ type: 'transport', action: 'next' })}><SkipForward size={16} /></button>
            </div>
            <div className="volume"><button className="icon-button" aria-label={state.volume === 0 ? 'Unmute' : 'Mute'} disabled={!connected} onClick={() => changeVolume(state.volume === 0 ? Math.min(lastVolume.current, state.max_volume) : 0)}><Volume2 size={17} /></button>
              <input aria-label="Volume" type="range" min="0" max={state.max_volume} step="0.01" value={Math.min(state.volume, state.max_volume)} disabled={!connected} onChange={event => changeVolume(Number(event.target.value))} />
              <output>{Math.round(state.volume * 100)}%</output>
            </div>
          </div>}
          {tab === 'config' && <div className="config-controls">
            <div className="visual-settings"><h2>Visualiser settings</h2>
              <label htmlFor="spacing">Line spacing <output>{spacing.toFixed(1)}</output></label>
              <input id="spacing" type="range" min="0.1" max="5" step="0.1" value={spacing} onChange={event => setSpacing(Number(event.target.value))} />
              <label htmlFor="bands">Frequency bands <output>{bands}</output></label>
              <input id="bands" type="range" min="8" max="128" step="1" value={bands} onChange={event => changeBands(Number(event.target.value))} />
              <button onClick={() => history.current.clear(displayedTheme.current.graphTheme)}><RotateCcw size={14} />Clear traces</button>
            </div>
            <div className="theme-controls"><h2>Theme</h2>
              <select aria-label="Theme preset" value={themeSelection.mode === 'cover' ? 'cover-art' : theme.name} onChange={event => {
                if (event.target.value === 'cover-art') changeTheme({ type: 'automatic' });
                else { const preset = presets.find(p => p.name === event.target.value); if (preset) changeTheme({ type: 'manual', theme: preset }); }
              }}>
                <option value="cover-art">Cover art</option><option value="" disabled>Custom colours</option>{presets.map(preset => <option key={preset.name}>{preset.name}</option>)}
              </select>
              {themeSelection.mode === 'cover' && <p className="theme-hint">{themeSelection.cover ? 'Colours from the current song’s cover.' : themeSelection.lastCover ? 'Keeping the previous cover colours until new ones arrive.' : 'Using Purple Dream while cover colours are unavailable.'}</p>}
              <div className="colors">{([
                ['uiTheme', 'primary', 'Primary UI'], ['uiTheme', 'secondary', 'Secondary UI'], ['uiTheme', 'background', 'Background'],
                ['graphTheme', 'line1', 'Graph start'], ['graphTheme', 'line2', 'Graph end'],
              ] as const).map(([group, key, label]) => <label key={key}>{label}<input type="color" aria-label={label} value={(theme[group] as Record<string, string>)[key]} onChange={event => color(group, key, event.target.value)} /></label>)}</div>
            </div>
          </div>}
          {tab === 'cartridge' && <div className="cartridge-controls">
            <div><h2>Song cartridge</h2><p>Share this view with its song link and settings embedded in the image.</p></div>
            <button className="create" disabled={!connected || !state.current_track || busy || reading} onClick={createCartridge}><HardDrive size={16} />{busy ? 'Creating cartridge…' : 'Create cartridge'}</button>
            <label className="cartridge-import">{reading ? 'Reading cartridge…' : 'Open cartridge image'}
              <input type="file" accept="image/png,image/jpeg,image/webp" disabled={reading || busy} onChange={event => {
                const file = event.currentTarget.files?.[0]; event.currentTarget.value = ''; if (file) void importCartridge(file);
              }} />
            </label>
            {imported && <div className="cartridge-result"><p>Song: {imported.videoId} · {imported.bands} bands</p>
              <button disabled={!connected || busy || reading} onClick={loadCartridge}><Play size={16} />Play and restore view</button></div>}
            {cartridge && <div className="cartridge-result" role="status">
              <a href={downloadUrl(cartridge.url, location.origin)!} download={cartridge.filename}>Save cartridge · {cartridge.filename}</a>
              {cartridge.storage === 'memory'
                ? <><p>In memory{cartridge.path && <> · <span>{cartridge.path}</span></>}</p><p>Available until the playback daemon stops or you remove it from staticfs. Save a copy to keep it.</p></>
                : <p>Saved to <span>{cartridge.path}</span></p>}
            </div>}
          </div>}
        </div>
      </div>
      {notice && <div className="notice" role="status"><span>{notice}</span><button aria-label="Dismiss message" onClick={() => setNotice(null)}>×</button></div>}
    </section>
  </main>;
}
