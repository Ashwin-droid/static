# Static Agent Notes

## Project Shape

Static is a personal YouTube Music client written in Rust. It is one app with several faces:

- an iced desktop GUI,
- a terminal UI,
- a small CLI surface,
- and a background playback daemon that owns the real playback session.

The daemon is the center of gravity. UI surfaces should be treated as clients that display and mutate daemon snapshots, not as independent playback owners. When a feature touches playback, queue state, repeat behavior, radio, pause/resume, MPRIS, PipeWire, or settings reloads, first understand how the daemon represents that state and how the UI mirrors it.

This repo is practical and personal. Prefer local, direct changes that match the current code over new layers or broad abstractions.

## Working Style

- Start by reading the nearby implementation. The source tree is the best reference for exact paths and helper names.
- Keep patches narrow and behavioral. The user usually wants the reported flow fixed, not a general cleanup pass.
- Do not preserve legacy behavior just for compatibility unless the user asks. This is primarily a single-user app, and clean semantic changes are often preferred.
- Treat the GUI, TUI, and CLI as related surfaces. If behavior exists in more than one surface, check whether the change needs parity.
- Placement matters in UI parity work. A queue action or status control being in the "same place" as another frontend is part of the requirement.
- Avoid replacing runtime investigation with source-only theories when the bug is about tracking, auth, playback state, or daemon behavior.
- Do not install, restart, or replace the live packaged app unless the user explicitly asks.

## Daemon Architecture

The daemon holds the live playback state, queue, current index, playlist context, radio virtual-playlist state, repeat mode, active track metadata, probe state, sponsorblock state, prefetch work, settings, MPRIS bridge, peer-player coordination, PipeWire coordination, volume ramps, and persisted playback snapshot.

Most clients talk to it through a snapshot/request model. A future agent should preserve that mental model: UI code should generally ask the daemon to perform playback mutations, then apply the returned snapshot. If UI code keeps temporary local state for responsiveness, make sure it eventually reconciles with daemon truth.

Daemon behavior is stateful in the desktop session. Passing tests or changing source files does not mean the live `/usr/bin/static --daemon` has been updated. Treat live-daemon changes separately from source changes.

## Radio And Virtual Playlists

Radio is not just "append recommendations to the queue." Static models radio as a queue plus a virtual-playlist boundary.

The radio virtual playlist records:

- the source track that seeds the radio,
- how much of the front of the queue has been intentionally touched by the user,
- and the radio filter chips associated with that seed.

That `touched` boundary is important. User-selected, reordered, inserted, or played tracks belong to the intentional prefix; generated radio recommendations belong to the tail. Queue operations should preserve or adjust that boundary instead of flattening radio into an ordinary queue.

Radio tail refreshes should dedupe against the prefix, preserve useful chip state, and avoid racing stale background fetches into a newer queue. Dismissing the queue after the current song also exits radio mode, because the generated tail and radio identity are no longer meaningful.

Repeat behavior also cares about the virtual-playlist boundary. Be careful when changing repeat-all, next/previous, natural-end behavior, or "jump to radio seed" flows: radio queues can be longer than the user-touched region, and those two concepts are intentionally separate.

## Seamless Listening

Seamless listening (`seamless_listening`, off by default) keeps music going from the offline library when a forward advance cannot fetch its next song. It is daemon behavior: frontends only render the result.

It covers forward advances only — natural track end, Next, and skipping past a failed stream. A song the user picked explicitly is never substituted; that failure stays a failure. The lookahead comes first: a next song that is already cached or prefetched just plays. A fill happens when the next song is not locally available and the network is known to be down, when its fetch fails, or when a radio queue reaches its end because the tail could not be extended. A playlist that simply finished still ends.

Fills are ordinary queue entries inserted next to the current track, so in radio mode they land inside the touched prefix. While offline, the unfetchable song stays ahead of the fill and is retried afterwards; when the network looks fine, the failed song itself is the suspect and the advance moves past it.

The pick samples the combined offline store (automatic, manual, and playlist downloads) weighted by offline score and the radio pick's artist/title repulsion. `Track::offline_fill` marks a fill per row, and every queue surface shows it: amber rows in the GUI and TUI, a suffix in `static queue get`, and the `offline_fill` origin in staticfs.

## Desktop Aware Volume

Desktop aware volume (`desktop_aware_volume`, off by default, needs wpctl and PipeWire ducking on — MPRIS coordination alone deliberately does not enable it, though MPRIS peer pauses count as interruptions once it is on) is daemon behavior in `src/desktop_volume.rs`: while any coordination has Static suppressed (PipeWire duck, PipeWire pause, or an MPRIS peer pause including the pending halt-auto window), it samples the default sink; when Static comes back it applies a self-compensation multiplier (sink level before the interruption minus the sink level now, in dB; the goal is Static's loudness staying what it was, not matching the other audio) so loudness matches the pre-interruption level, holds, then slides the sink back to baseline while fading the compensation out together.

Ground rules that must survive changes: the job is bound to the sink node it started on and a default-sink switch abandons it; once Static is alone, a sink change it did not write itself is absorbed rather than fought — the compensation and the delta still to be undone stay exactly as they were and the user's move shifts the end point by the same dB (crank 10→50, user bumps to 60 while playing: the restore still drops 40 and lands on 20; the same during the slide), while muting is the one touch that abandons; abandoning only fades the compensation and never touches the sink; the hold/restore clocks only run while Static is actually playing alone; a user pause freezes the job (never abandons it) and a sink change made during the pause re-bases the compensation on resume. Downward cranks count too, so compensation can be positive. The compensation lives in the daemon's output-volume math next to the duck multiplier, not in dgain/xdgain.

## Session Lock Pause

Static pauses itself when the desktop session locks or the screen blanks (`src/session_lock.rs`). This is unconditional: there is deliberately no setting for it on any surface. The watcher listens for login1 `Lock`/`LockedHint`/`IdleHint` on the system bus and `ScreenSaver.ActiveChanged(true)` on the session bus, and the daemon drains the queued edges in its maintenance tick.

The pause is edge-triggered and behaves exactly like a user pause (it cancels a pending halt-auto resume and stops waiting on peers). Nothing holds the pause: if the user presses Play from the lock screen that is their choice, and unlocking does not resume. Do not turn this into a level-held "locked means silent" state.

## MPRIS And PipeWire Coordination

Playback coordination is one of the most sensitive parts of the app.

Static coordinates with two kinds of external audio activity:

- MPRIS peer players, such as other media apps,
- and PipeWire activity, such as non-MPRIS audio streams from games, calls, or browsers.

PipeWire ducking is dynamic: the daemon taps each coordinated foreign stream with a native PipeWire capture stream on its own loop thread (`src/pipewire_tap.rs`; never fall back to another target, or the tap hears Static's own music or the mic), measures 20 ms windows, and maps the combined level through a fixed ease-in-out curve from `pipewire_ducking_min` (quiet foreign audio) to `pipewire_ducking_max` (loud foreign audio). The level window is fixed (about -50 to -20 dBFS short-term): there is deliberately no speech detection and no adaptive range. Ducking deeper is instant, from any depth: a louder window wakes the daemon to apply it without waiting for the maintenance tick. Climbing back is the slow side, `pipewire_ducking_gain_back_secs` (default 5) for the whole loud-to-quiet climb, and the unduck when the foreign stream ends ramps over the same time. While a new tap is still connecting the duck holds; with no live tap it sits at the maximum. Desktop aware volume multiplies on top of the duck output. Static's sink ring buffer is kept short on purpose (`AUDIO_SINK_BUFFER_TIME_US`), because every volume change lands that much later; it is separate from the download/seek buffering.

The app has coordination modes like off, halt, and halt-auto. The old `respect` mode is no longer a separate user-facing concept; its behavior has been folded into halt-auto semantics, with zero delay meaning immediate resume.

MPRIS halt-auto has a deliberate side channel. During the pending auto-resume window, Static may advertise itself as playing to external media controls even though actual audio is still paused/frozen. That is an intent-capture trick: MPRIS Play/Next/Seek style commands during the pending window mean "resume Static now" without necessarily applying the transport command literally. Pause-like commands cancel the pending resume.

Do not simplify this into ordinary play/pause handling. The side channel exists to let external controls express intent while preserving Static's paused audio state until the policy decides what to do.

PipeWire coordination has its own priority relationship with MPRIS coordination. When both are involved, keep the intended order in mind: MPRIS pause takes precedence over PipeWire pause, which takes precedence over PipeWire ducking. Bugs in this area often look like "the UI says paused but audio is still playing" or "auto resume/cancel buttons linger after the decision is already made."

Pending halt-auto controls in the GUI and TUI should be driven by daemon snapshots. When the daemon decides, times out, resumes, or cancels, stale pending UI should disappear promptly.

## Settings Model

Settings are shared product behavior, not just preferences UI.

There are three settings surfaces:

- GUI settings,
- TUI settings,
- and CLI config commands.

The CLI intentionally supports some wider/advanced values than the GUI settings cards. When a saved value is outside the UI-safe range, the UI should communicate that it is CLI-only rather than silently clamping it away. Do not "fix" this by forcing every setting through the GUI range.

Settings that affect playback coordination, cookies, offline cache behavior, prefetching, SponsorBlock, storage roots, or Chromium cookie sync can have daemon/runtime consequences. If a setting changes while the daemon is running, make sure the daemon reload path or equivalent state sync remains intact.

## Network, Auth, Tracking, And Offline State

Static depends on a saved YouTube Music session. The GUI app is the normal way to establish auth; CLI and TUI flows can bootstrap from saved cookies or configured Chromium cookie sync.

Tracking bugs should be investigated with runtime evidence. A successful local play row or a successful-looking tracking request does not necessarily prove YouTube Music credited the listen. When the user reports tracking/auth behavior, inspect logs, stored auth/session state, and the real request/response path before making a source-only fix.

Offline cache and storage-root behavior are part of the product, not incidental persistence. Moving storage roots, cache management, listen-again behavior, prefetching, and local playback preference should be handled carefully because they affect both startup and playback.

A storage-root move only ever reads the old root, because it can be a read-only mount (an NTFS partition after an unclean Windows shutdown): files it won't let go of are copied, and the database is snapshotted with `VACUUM INTO` and rewritten on the new side. The frontend that moved it then asks the daemon to reload, and the reload reopens the daemon's cache; without that the daemon keeps writing to the old, deleted database until it restarts.

## UI Parity

The GUI and TUI should feel like two views over the same app, not separate products.

Areas that commonly need parity:

- search behavior and when input clears,
- queue actions and where they appear,
- radio chips and radio seed affordances,
- pending halt-auto resume/cancel controls,
- settings ranges and CLI-only explanations,
- repeat and queue behavior in radio mode.

When implementing a feature on one surface, look for the matching concept on the other surface. If you intentionally leave one surface unchanged, call that out.

## staticfs

staticfs is the fourth client: a FUSE filesystem (default `~/Music/staticfs`) that exposes the daemon as plaintext files. It is a compile-time option — the `staticfs` cargo feature gates all of it, and the deb package builds with it enabled.

The daemon owns the FUSE driver. There is no `static fs` command; the daemon hosts the mount in-process, controlled by the `staticfs_enabled` setting (GUI/TUI toggle, `static config`, or `settings/staticfs_enabled` inside the fs itself — the setting hot-reloads, so toggling mounts/unmounts the live filesystem). `staticfs_mountpoint` is a CLI-only setting. Lifecycle invariants: daemon death takes the mount with it; a dead or externally-unmounted session does NOT take the daemon down — the host respawns it with backoff while the setting is on. When an unmount is blocked by open handles, the daemon publishes a pending state (`staticfs_unmount_pending` in snapshots) and the GUI/TUI show "an app is using {file}" with close-anyway/wait controls; the CLI and the fs deliberately show no prompt. Close-anyway lazy-detaches (straggling handles get ENOTCONN).

Even though the mount lives in the daemon process, staticfs stays architecturally a *client*: it talks to the daemon over the same D-Bus request model as any other frontend, and it has no state of its own. Every read materializes content at `read()` time; every write forwards a command to whoever owns the data. Live state (`playback/`, `settings/`) goes over the daemon D-Bus request model; historical stats (`stats/`) are read-only SQL against the shared data.db. Do not add caching or local models beyond the short stats TTL; the daemon and database stay the source of truth.

Rejection is the sysfs model and it must stay genuine: invalid writes fail with EINVAL because the daemon (or the local write grammar) refused them, and there is no disk state a privileged user could force instead. Daemon-rejected and transport-failed calls are deliberately distinct (EINVAL vs EIO). staticfs must never spawn the daemon.

The `settings/` directory listing derives from the daemon's reply, so new settings appear without staticfs changes. Several playback files carry play semantics on write (track/video_id play like `exec`; queue and next writes resolve free text through the daemon's own resolver at the CLI insert slots; video_name plays like `exec` but ALWAYS resolves via search — writing there states "this is a song name", never a bare video id) — when changing queue or radio behavior, check whether the staticfs write surfaces need parity the same way the GUI/TUI do. The root `shutdown` file reads as a liveness one-liner and shuts the daemon down (taking the fs with it) when exactly "yes" is written.

Known limits are intentional: no per-play event log exists, so time-bucketed stats are approximations over `last_played`; and FUSE cannot emit inotify events for daemon-side changes, so followers must poll (uutils tail needs `--use-polling`). Do not "fix" these by adding new persistence or fake notification schemes without a deliberate design.

## Testing And Validation

This is a Rust project with module-local tests. The crate is binary-oriented, so do not assume a conventional library-test layout.

Tests must never see the user's library. Opening a cache at a temp root while a real default `data.db` exists counts as a storage move, and the move empties the live library into the test's temp root, which the test then deletes. Under `cfg(test)` the default root therefore points at a per-process temp dir. Never write off "cache init" test failures as flaky: that was exactly this wipe, twice.

Use focused checks for the area changed, then broader formatting/tests when the change has behavioral risk. Daemon coordination, radio queue semantics, settings parsing, and UI parity helpers deserve tests because small state mistakes can be hard to see manually.

If you cannot run the relevant checks, say so clearly in the final response.

## Scope Of This File

This file should preserve the concepts and product invariants future agents are likely to miss. It should not become a line-number map, a command cookbook, or a second README for every module. Exact commands, helper names, and call paths should be re-explored from the current checkout when needed.
