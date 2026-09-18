# Static visualiser

Local adaptation of [Ashwin-droid/echo-trace-visions](https://github.com/Ashwin-droid/echo-trace-visions), commit `d2ef71a451b4bab3c56e5a1ecf5fcf3cfcace824`. The frequency-trace geometry, camera mapping, initial view, purple control panel and twelve theme presets come from that project. Fabric mode is omitted.

Static bundles `dist/` into its Rust binary and opens this page in a separate local window. All scripts, styles and fonts are local. There are no CDN requests or remote site frames. System fonts are used.

The daemon owns playback and analysis. The page receives bounded frequency values through a local authenticated WSS connection, and sends playback controls back to Static. It does not load, decode, analyse or play audio. History has a fixed allocation of at most 128 bands by 350 samples, advances only on fresh frame sequences, freezes while paused, and keeps the previous song’s traces across track changes and reconnects. Old samples roll out naturally; Clear traces or changing the band count clears the history. Dragging and theme changes remain available while paused.

The default Cover art theme follows the current song. Rust extracts its palette and sends five RGB colours; the page never downloads or decodes cover images. The last valid cover palette stays visible while a new palette is pending or unavailable. Purple Dream is used only before the first valid cover palette. Selecting any of the twelve presets or editing a colour keeps that manual choice across songs. Select Cover art again to follow the current artwork. Cartridges include the colours currently displayed.

Cartridge creation captures a PNG of the view, at most 1280 by 720, and sends that screenshot plus visual preferences and the selected video ID. Rust embeds a compact binary song link and view payload into the card's image pixels with the `blind_watermark` DWT–DCT–SVD library and Reed–Solomon error correction. No audio is downloaded, decoded or included. Titles and capture time are visible text on the card; they are not part of the binary payload.

In the Cartridge tab, choose **Open cartridge image**, then **Play and restore view**. Rust reads the image and recovers its settings. Playback goes through the daemon's normal YouTube Music resolver and requires the song to be available to that account or cache. Opening a file alone does not change playback. The restored view is quantized; the image itself preserves the original traces.

For inspection without starting the daemon or changing playback, run:

```sh
static visualizer decode /path/to/cartridge.jpg
```

The decoder prints the reconstructed YouTube Music link and view settings as JSON. PNG, JPEG and WebP input is supported, with a 16 MiB input limit and bounded decoded dimensions. The old audio-appended cartridge format is no longer exported or imported.

## Moving the view

The Camera panel is replaced by gestures on the visualiser itself:

- Drag with the left mouse button to rotate the view.
- Hold W/S to move forward/backward, A/D to strafe left/right, and Q/E to move down/up. Movement follows the view direction, with Q/E using the scene’s vertical axis. It stops when you release the keys or focus a control.
- Shift-drag or right-drag to pan; middle-drag or the scroll wheel zooms.
- Double-click to reset. A focused canvas also supports arrow-key panning and Home to reset.
- Touch uses one finger to rotate and two fingers to pan and pinch-zoom.

View movement runs outside React's render state. Cartridges capture the actual target, orbit angles and zoom distance from the frame they show.

## Fullscreen

Use the fullscreen button or F11 for a decoration-free view with the controls hidden. Drag, pan, pinch and scroll gestures remain active. The small sliders button in the upper-right corner reveals or hides the controls. Escape or F11 exits fullscreen and restores the regular layout.

## Opening the visualiser

Use the Visualiser button above Settings in the GUI, press `v` in the TUI, or run `static visualizer`. This opens a separate native WebKitGTK window. The daemon and frontend must both use the current build. Closing the window closes its local service and analysis subscription.

The Rust host requires the WebKitGTK 4.1 and GTK 3 development libraries when compiling. Its installed runtime dependencies are collected by `cargo deb`. Compiled frontend files are embedded in the executable, so running or installing it does not require Node or a web server.

When staticfs is enabled, new cartridges live only in the playback daemon's memory. Rust streams the finished watermarked PNG into a bounded upload without creating an output file or temporary file. Completed cartridges appear at `<staticfs mount>/cartridges/<filename>.png`; if the mount is temporarily unavailable they remain in memory, with no disk fallback. Closing the visualiser leaves them available until the daemon stops. Removing a memory cartridge from staticfs releases it. Use **Save cartridge** or copy the file out of staticfs to keep a permanent copy. The explicit save link streams into `~/Downloads/Static/` in the native window; nothing is downloaded automatically.

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
- `{ "type": "play_cartridge", "video_id": "dQw4w9WgXcQ" }`, resolving and playing a decoded song through the daemon.
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


## Colour transitions

Every valid palette change blends the colour used for newly arriving FFT points, UI colours and the background over three seconds. Existing trace points keep their recorded colours, so the transition becomes a visible gradient segment along each trace. Red, green and blue each interpolate independently from their old 0–255 value to their new value on every animation frame, with no intermediate stages. Yellow fading to blue therefore passes smoothly through grey. An interrupted fade starts its next transition from the current on-screen blend. The existing animation loop drives the fade without rendering the React controls on every frame.

Missing or delayed artwork keeps the previous cover palette indefinitely; only a valid new palette starts another transition. Cartridge images preserve the multi-colour trace history and the currently displayed background blend.

## Binary cartridge format, version 1

The positional payload is 39 bytes, with no separators, JSON, hex text, filenames or full URL. All multibyte integers use big-endian order.

| Byte offsets | Meaning | Encoding |
| --- | --- | --- |
| 0–1 | Format marker | ASCII `SC` |
| 2 | Version | `1` |
| 3–11 | YouTube video ID | Eleven URL-safe base64 alphabet indices packed into 66 bits; top six bits zero |
| 12–26 | Five colours | Three RGB bytes each: UI primary, secondary, background, graph start, graph end |
| 27–29 | Camera target X, Y, Z | Signed bytes, two scene units per step; −256 through 254 |
| 30 | Camera yaw | One turn divided into 256 steps |
| 31 | Camera polar angle | Zero through π, divided into 255 steps |
| 32 | View distance | Unsigned byte, 5–255 |
| 33 | Line spacing | Unsigned byte, 0.05 units per step |
| 34 | Frequency bands | Unsigned byte, 8–128 |
| 35–38 | CRC-32 | Checksum of bytes 0–34 |

The orbit camera's unused third rotation value is zero and is omitted. Position precision is within one scene unit, distance within half a unit, yaw within about 0.71°, and polar angle within about 0.36°. Out-of-range views are rejected with an instruction to move closer or reduce distance; values never wrap. Colours and the ID are lossless.

`reed-solomon` adds 128 parity bytes to the 39-byte payload, producing 167 bytes and correcting up to 64 corrupted byte symbols. The watermark library embeds this codeword with its own repeated-bit placement and three-channel voting. We use `blind_watermark` **0.1.3**, primary singular-value strength 72, secondary embedding disabled, strategy seed `0x5354415449430001`, and LSB-first bit order. The public seed is a format parameter, not an encryption key. No watermark algorithm, transform, placement strategy or ECC implementation is maintained in Static.

Exports are 800×1120 pixels. A received image with the same aspect ratio is normalized to that size for recovery. Tests cover JPEG quality 90/70/50/30, resizing to 600×840 and 400×560 followed by JPEG quality 70, and repeated JPEG encoding. These are local recompression tests, not proof of every WhatsApp pipeline. Keep the full image: cropping, added borders, rotation or severe editing can prevent recovery. The stronger watermark can slightly alter flat regions. The version marker, field validation and checksum must all pass before recovered data is offered to the user.

`POST /cartridge/decode?token=…` accepts the raw image with the same loopback Host, Origin and token checks as the WebSocket. It runs on the bounded native cartridge worker and returns only recovered link/view metadata. It does not write the uploaded image to disk.

## dGain calibration

Run `static settings dGain calibrate` to open the separate 900×760 diagnostics window. It uses the same bundled native WebKit host, with a lightweight canvas page instead of loading the Three.js visualiser. It requires a daemon built with diagnostics support; opening it does not enable dGain or change settings.

The upper graph shows decoded song RMS, dGain's weighted rolling RMS and processed output RMS in dBFS. Processed output includes dGain correction against WRA, including any previous-song reference, before master volume and its limiter, so changing the listening volume cannot move this line. Separate final-output and final-peak readouts include master volume and limiting. Its shaded region is the permitted deviation around the average. The lower graph uses gain dB and separates requested correction, conservative lookahead correction, applied dGain and limiter attenuation. These values come from the real PCM processor, not a browser simulation. The original block's average and input levels travel with it through the 250 ms lookahead queue; gain-output RMS/peak are measured before master volume and limiting, and final RMS/peak from the samples actually emitted by the filter. This uses a separate measurement, not division by master volume, so it remains valid when muted or limited. Applied gain is the envelope at the end of that approximately 10 ms block. Device mixer levels are downstream and are not measured here.

The window polls daemon diagnostics at 10 Hz, retains at most 600 measurements / 60 seconds, rejects duplicates, and retains graph history across seeks, stream resets and setting changes. A new song starts a fresh graph; a seek breaks the plotted line at the position jump instead of erasing earlier samples. Pause freezes incoming graph data. Freeze graph holds a view for inspection while numeric meters remain live; hover either plot to inspect recorded values. Clear graph only clears the display. Extra readouts show peak level, master volume, limiter activity, the previous song's fixed WRA reference, the WRA response time and audible audio observed since reset, lookahead, attack/release time and the correction strength. Silence holds the WRA reference and does not pull it toward zero. The first eligible audible block after the 15-second opening guard seeds WRA on an unanchored song; chained songs seed it from the previous song’s final WRA. The upper graph includes that fixed reference in dBFS. When dGain is off, input/output and limiter monitoring continue, but there is no active rolling reference.

The settings form edits dGain, flatten tolerance, averaging time, change lazyness and xdGain. **Apply settings** validates the whole edit against the CLI's existing ranges, loads fresh settings to preserve unrelated concurrent changes, saves once, and hot-reloads the daemon once. Edits persist in normal playback settings. Turning dGain off in the form also unchecks xdGain. Opening or closing the window does not write preferences or reset the xdGain reference.

The calibration subscription uses authenticated loopback WSS at `/gain/ws`, a persistent D-Bus client on a separate worker, bounded channels, and latest-value delivery for slow renderers. It does not subscribe to FFT frames, load artwork, or play audio itself. Diagnostics update inside the existing DSP lock with fixed-size fields; the browser never locks or owns playback. There is no additional audio buffering or gain stage.

Change lazyness has two continuous effects. It scales gain correction in dB by `1 - lazyness / 100`, using fixed 35 ms attack and 250 ms release smoothing. It also shifts WRA's input/history split smoothly from 90/10 at 0% to 10/90 at 100%. At 100%, applied gain correction is zero, but WRA keeps evolving slowly and the enable switches stay unchanged. Master volume and the final clipping limiter remain active. The calibration readouts show both the correction strength and the effective WRA weights.

The average blends in **dBFS**, with existing settings, stored values and ranges unchanged. Let `L = lazyness / 100` and `H = 0.1 + 0.8 × L`. Without a previous-song reference, the weights over one RA interval are `input = 1-H`, `previous WRA = H`. With xdGain active and reference `X`, reserve exactly 20% for `X`; the remaining weights are `input = 0.8 × (1-H)` and `previous WRA = 0.8 × H`. Thus the chained weights are 72/20/8 at 0%, 40/20/40 at 50%, and 8/20/72 at 100%, in input/reference/history order. xdGain does not contribute on the first song.

At a song's end or skip, its last emitted WRA becomes the next song's fixed reference. The next song seeds its WRA from that reference. `X` remains fixed throughout the song and is replaced at the next boundary. There is no separate xdGain volume offset. Silent/empty songs cannot create a new reference. Toggling xdGain off clears its chain; it does not discard the current song's measured WRA timeline.

For fractional intervals, `retention = history_weight^(block_seconds / dgain_ra_seconds)` and `destination = (input_weight × input_dBFS + reference_weight × X) / (1-history_weight)`. Update `WRA_next = retention × WRA_previous + (1-retention) × destination`. This reproduces the selected weights over one RA interval with constant input, independently of block size and sample rate.

## Opening guard and seeking

Media positions **0–15 seconds** do not update WRA. With an inherited reference it stays fixed during that opening; without one, dGain passes the input through. Master volume and clipping protection remain active. The guard uses media timestamps, so seeking past 15 seconds does not start a new wait. The audio filter splits a buffer at the boundary when necessary. Silence continues to hold the reference after the guard.

WRA checkpoints are kept per song in media time, normally about every 100 ms. Seeking into a visited region restores/interpolates that recorded curve instead of averaging the same passage repeatedly. Seeking into an unvisited region continues from the nearest earlier recorded WRA, or the previous-song reference if no earlier measurement exists. GStreamer flushes, segments and format changes clear buffered PCM and gain envelopes, but retain WRA checkpoints. Settings affect subsequent evolution without erasing recorded values. A new song clears its predecessor's timeline after handing off the final WRA through xdGain.

The timeline is bounded to 65,536 checkpoints. Very long tracks progressively reduce checkpoint resolution, preserving early and late regions rather than evicting the beginning. Large unvisited gaps are not interpolated. The calibration graph retains its last minute across seeks and shows an opening-guard countdown and whether it is holding a reference or passing through. `opening_guard_millis` reports the guard remaining at the emitted block's position; `xdgain_reference_dbfs` reports the fixed previous-song reference or null.
