# Static visualiser

Local adaptation of [Ashwin-droid/echo-trace-visions](https://github.com/Ashwin-droid/echo-trace-visions), commit `d2ef71a451b4bab3c56e5a1ecf5fcf3cfcace824`. The frequency-trace geometry, camera mapping, initial view, purple control panel and twelve theme presets come from that project. Fabric mode is omitted.

Static bundles `dist/` into its Rust binary and opens this page in a separate local window. All scripts, styles and fonts are local. There are no CDN requests or remote site frames. System fonts are used.

The daemon owns playback and analysis. The page receives bounded frequency values through a local authenticated WSS connection, and sends playback controls back to Static. It does not load, decode, analyse or play audio. History has a fixed allocation of at most 128 bands by 350 samples, advances only on fresh frame sequences, freezes while paused, and resets on playback generation changes. Dragging and theme changes remain available while paused.

The default Cover art theme follows the current song. Rust extracts its palette and sends five RGB colours; the page never downloads or decodes cover images. Purple Dream is the fallback when a palette is pending or unavailable. Selecting any of the twelve presets or editing a colour keeps that manual choice across songs. Select Cover art again to follow the current artwork. Cartridges include the colours currently displayed.

Cartridge creation captures a PNG of the view, at most 1280 by 720, and sends that screenshot plus visual preferences and the selected video ID. Rust prepares the artwork and embeds the audio. The returned download is an ordinary browser download streamed from Rust, without fetching the cartridge into a JavaScript blob. Its storage status and, when available, its filesystem path are shown alongside it. Original browser cartridge import code is intentionally absent.

## Moving the view

The Camera panel is replaced by gestures on the visualiser itself:

- Drag with the left mouse button to orbit the view.
- Shift-drag or right-drag to pan; middle-drag or the scroll wheel zooms.
- Double-click to reset. A focused canvas also supports arrow-key panning and Home to reset.
- Touch uses one finger to rotate and two fingers to pan and pinch-zoom.

View movement runs outside React's render state. Cartridges capture the actual target, orbit angles and zoom distance from the frame they show.

## Fullscreen

Use the fullscreen button or F11 for a decoration-free view with the controls hidden. Drag, pan, pinch and scroll gestures remain active. The small sliders button in the upper-right corner reveals or hides the controls. Escape or F11 exits fullscreen and restores the regular layout.

## Opening the visualiser

Use the Visualiser button above Settings in the GUI, press `v` in the TUI, or run `static visualizer`. This opens a separate native WebKitGTK window. The daemon and frontend must both use the current build. Closing the window closes its local service and analysis subscription.

The Rust host requires the WebKitGTK 4.1 and GTK 3 development libraries when compiling. Its installed runtime dependencies are collected by `cargo deb`. Compiled frontend files are embedded in the executable, so running or installing it does not require Node or a web server.

When staticfs is enabled, new cartridges live only in the playback daemon's memory. Rust streams the card, full encoded audio, and metadata into a bounded upload without creating an output file or temporary file. Completed cartridges appear at `<staticfs mount>/cartridges/<filename>.png`; if the mount is temporarily unavailable they remain in memory, with no disk fallback. Closing the visualiser leaves them available until the daemon stops. Removing a memory cartridge from staticfs releases it. Use **Save cartridge** or copy the file out of staticfs to keep a permanent copy. The explicit save link streams into `~/Downloads/Static/` in the native window; nothing is downloaded automatically.

With staticfs disabled, new cartridges are saved atomically in the daemon's current storage root under `cartridges/`. Existing disk cartridges are preserved. Staticfs serves both existing files and memory cartridges using bounded byte ranges; cartridge content is read-only. Daemon memory has a shared 512 MiB limit for pending and completed cartridges, and a full store rejects the export without writing to disk.

## Building

Use Node 22.12 or newer. The lockfile pins dependencies.

```sh
cd visualizer
npm ci
npm test
npm run build
```

`npm run build` first checks TypeScript. Commit the resulting `dist/` together with source changes so regular Rust/package builds do not need Node or network access. `node_modules/` is ignored. Rebuilding the Rust binary is necessary after updating the bundled assets.

## Protocol

The launch URL supplies `#token=…`. The client connects to `wss://${location.host}/ws?token=…`. It never stores the token in local storage and never connects to another host. The local Rust service validates the token and the HTTPS origin. Downloads use the same HTTPS origin under `/cartridges/`.

Client messages:

- `{ "type": "configure", "bands": 30 }`, with 8 to 128 bands.
- `{ "type": "transport", "action": "toggle" }`, or `previous` or `next`.
- `{ "type": "volume", "value": 0.8 }`, using the daemon's `max_volume` limit.
- `{ "type": "cartridge", "id": "uuid", "video_id": "…", "screenshot": "data:image/png;base64,…", "metadata": { "uiTheme": {}, "graphTheme": {}, "cameraControls": { "position": {}, "rotation": {} }, "lineSpacing": 2, "fftSize": 30, "timestamp": 0 } }`.

Server messages:

- `{ "type": "state", "state": PlaybackSnapshot }`.
- `{ "type": "theme", "video_id": "…", "theme": { "name": "Cover art", "uiTheme": { "primary": "#aabbcc", "secondary": "#bbccdd", "background": "#112233" }, "graphTheme": { "line1": "#aabbcc", "line2": "#ddeeff" } } }`. `theme: null` clears a pending or unavailable palette. `video_id` can be null when no song is selected. The client discards palettes for other songs.
- `{ "type": "audio", "frame": { "generation": 1, "sequence": 2, "sample_rate": 48000, "bands": [], "rms": 0.1, "peak": 0.5 } }`. Bands are normalized to 0–1. Generation and sequence increase monotonically during one server lifetime.
- `{ "type": "error", "message": "…", "id": "optional request id" }`.
- `path` is `null` for memory cartridges when no live staticfs mount is available; disk cartridges always include their saved path. `storage` is `"memory"` or `"disk"`.
- `{ "type": "cartridge_ready", "id": "…", "filename": "…", "url": "…", "storage": "memory", "path": "…" }`.

The client reconnects with capped exponential backoff, at most eight consecutive retries. Closing the window cancels the socket, retry and connection timeout. Commands are not queued across disconnects. WebGL cleanup cancels the render loop, removes the resize observer and disposes every geometry, material, renderer and context.

## Validation

The unit tests exercise protocol validation, fixed memory history, stale/duplicate frame rejection, track generation changes, pause behavior, boosted volume snapshots, download origin restrictions, socket backpressure, connection timeout and cleanup. Browser checks should also cover the native window's certificate trust, audible playback driving the traces, pause freezing them, camera/theme changes, volume control, and a cartridge download through the Rust service.
