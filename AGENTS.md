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

## MPRIS And PipeWire Coordination

Playback coordination is one of the most sensitive parts of the app.

Static coordinates with two kinds of external audio activity:

- MPRIS peer players, such as other media apps,
- and PipeWire activity, such as non-MPRIS audio streams from games, calls, or browsers.

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

staticfs is the fourth client: a FUSE filesystem (mounted with `static fs`, default `~/Music/staticfs`) that exposes the daemon as plaintext files. It is a compile-time option — the `staticfs` cargo feature gates all of it, and the deb package builds with it enabled.

The core invariant is that staticfs is a file-shaped proxy with no state of its own. Every read materializes content at `read()` time; every write forwards a command to whoever owns the data. Live state (`playback/`, `settings/`) goes over the existing daemon D-Bus request model; historical stats (`stats/`) are read-only SQL against the shared data.db. Do not add caching or local models beyond the short stats TTL; the daemon and database stay the source of truth.

Rejection is the sysfs model and it must stay genuine: invalid writes fail with EINVAL because the daemon (or the local write grammar) refused them, and there is no disk state a privileged user could force instead. Daemon-rejected and transport-failed calls are deliberately distinct (EINVAL vs EIO); a daemon that is down or too old degrades to EIO, never to silent success. staticfs must never auto-spawn the daemon.

The `settings/` directory listing derives from the daemon's reply, so new settings appear without staticfs changes. Several playback files carry play semantics on write (track/video_id play like `exec`; queue and next writes resolve free text through the daemon's own resolver at the CLI insert slots) — when changing queue or radio behavior, check whether the staticfs write surfaces need parity the same way the GUI/TUI do.

Known limits are intentional: no per-play event log exists, so time-bucketed stats are approximations over `last_played`; and FUSE cannot emit inotify events for daemon-side changes, so followers must poll (uutils tail needs `--use-polling`). Do not "fix" these by adding new persistence or fake notification schemes without a deliberate design.

## Testing And Validation

This is a Rust project with module-local tests. The crate is binary-oriented, so do not assume a conventional library-test layout.

Use focused checks for the area changed, then broader formatting/tests when the change has behavioral risk. Daemon coordination, radio queue semantics, settings parsing, and UI parity helpers deserve tests because small state mistakes can be hard to see manually.

If you cannot run the relevant checks, say so clearly in the final response.

## Scope Of This File

This file should preserve the concepts and product invariants future agents are likely to miss. It should not become a line-number map, a command cookbook, or a second README for every module. Exact commands, helper names, and call paths should be re-explored from the current checkout when needed.
