# Playlist management

Static can create private YouTube Music playlists, save the current radio queue
as a playlist, add songs, remove selected entries, reorder entries, and delete
playlists. Listen again pin/unpin controls send account updates to YouTube Music.

## Controls

| Action | GUI | TUI |
| --- | --- | --- |
| Create empty playlist | Your Playlists → New playlist | Playlists → P: Create |
| Save radio | "Save as playlist" in the radio strip under the queue tabs | S: Save radio beside the queue controls |
| Add song | Song context menu → Add to playlist | Song context menu → Add to playlist |
| Create playlist containing a song | Type a name in the picker's "start a new one" field | N in the destination picker |
| Remove songs | Right-click a row on the playlist page → Remove from playlist | P: Edit → Space to select → Delete |
| Reorder | Drag a row on the playlist page and drop it on the row it should replace (downwards lands after it, upwards before it) | P: Edit → [ / ] |
| Delete playlist | Right-click the playlist card, or Delete on the playlist page → confirmation dialog | P: Edit → D → type DELETE → Enter |
| Pin / unpin Listen again | Song menu, playlist-card menu, or Pin/Unpin on the playlist page | Song/queue menu, or L / U in playlist editor |

In the GUI the playlist page itself is the editor: there is no separate edit
mode. Rows only reorder or remove on the account's own playlists (the page
loads the occurrence list in the background after the tracks); other people's
playlists stay read-only. A drag that leaves the page still drops into the
queue as before. Moves and removals apply locally once YouTube Music confirms
the edit; a rejected edit reloads the playlist.

Radio saving sends the daemon snapshot's complete current queue in order. It
does not replace the queue or change radio mode. YouTube can deduplicate repeated
video IDs during creation. Playlist editors address existing song occurrences,
so repeated copies already in a playlist remain distinct. YouTube may reject an
add when the song is already present; Static reports the rejection.

## CLI

```text
static playlist list
static playlist show <id>
static playlist create <title>
static playlist from-radio <title>
static playlist add <id> <URL|video id|search term>
static playlist remove <id> <position> [position ...]
static playlist move <id> <from> <to>
static playlist delete <id> [--yes]
static playlist pin <id>
static playlist unpin <id>
static pin [song input]
static unpin [song input]
```

Positions are 1-based, as printed by `show`. A move's destination is its final
position. `from-radio` requires an existing daemon with an active radio queue.
All mutations require the account's saved YouTube Music session. The server
remains authoritative about which playlists can be edited.

Deletion without `--yes` previews the named target and confirmation command.
Deletion is permanent; it does not clear the active playback queue or local
offline files. Song pin/unpin without an argument targets the existing daemon's
current song.

## Listen again verification

Pinning is remote account behavior, separate from offline retention. Static
resolves fresh feedback tokens, checks the processing acknowledgement, and
reloads remote state. It prefers the exact matching card in
`FEmusic_listen_again` because `/next` can report a stale song pin state, then
falls back to the item's own header or queue menu when the card is absent.
Absence from recommendations is not treated as proof of unpinning.

YouTube can acknowledge a pin before updated state is readable. Static polls
the reads for a bounded period and never retries the mutation automatically.
New populated playlists can similarly take time to become readable after
creation; their follow-up reads are retried without repeating creation.

**Known upstream limitation:** YouTube Music can report an unpin as processed
and show an unpin toast while the item remains pinned after fresh API reads and
Chrome reloads. This was reproduced live with the saved keyring session and
Chrome, and is also [documented by ytmusicapi](https://ytmusicapi.readthedocs.io/en/1.12.2/reference/api/ytmusicapi.mixins.html).
Static reports that the item is still pinned instead of displaying a successful
local-only unpin. It does not substitute negative recommendation feedback.

## Request evidence

Verified against YouTube Music in external Chrome on 2026-09-05. Common client
context and authentication are supplied by Static's existing network client.
The examples below omit account identifiers, authentication, and tracking data.

All endpoints are POSTs under `https://music.youtube.com/youtubei/v1/`.

| Operation | Endpoint | Relevant body fields |
| --- | --- | --- |
| Empty private playlist | `playlist/create` | `title`, `params: "KAA%3D"` |
| Private playlist from radio | `playlist/create` | Same fields plus ordered `videoIds` |
| Song/radio destination picker | `playlist/get_add_to_playlist` | `videoIds`, `excludeWatchLater: false` |
| Destination listing without a song | `playlist/get_add_to_playlist` | `playlistId: "LM"`, `excludeWatchLater: false` |
| Add | `browse/edit_playlist` | `playlistId`, `actions: [{action: "ACTION_ADD_VIDEO", addedVideoId}]` |
| Remove | `browse/edit_playlist` | `playlistId`, `actions: [{action: "ACTION_REMOVE_VIDEO", setVideoId, removedVideoId}]` |
| Move before another entry | `browse/edit_playlist` | `playlistId`, `actions: [{action: "ACTION_MOVE_VIDEO_BEFORE", setVideoId, movedSetVideoIdSuccessor}]` |
| Move to end | `browse/edit_playlist` | Same move action with no `movedSetVideoIdSuccessor` |
| Delete playlist | `playlist/delete` | `playlistId` |
| Pin / unpin | `feedback` | `feedbackTokens: [serverToken]`, `isFeedbackTokenUnencrypted: false`, `shouldMerge: false` |

The picker rejects an empty `videoIds` array with HTTP 400. The `LM` playlist
context was separately verified through Static's saved-session client and
returns the editable destinations without needing a selected song.

Creation returns `playlistId`. Edits return `status: "STATUS_SUCCEEDED"` on
success. Adding also returns `playlistEditResults` containing
`playlistEditVideoAddedResultData` with `videoId` and `setVideoId`. HTTP success
alone is insufficient: an edit returning `STATUS_FAILED` must be reported as a
failure.

Pin/unpin tokens come from the item's `toggleMenuServiceItemRenderer`, using
the `KEEP` and `KEEP_OFF` icons to identify the commands independently of
localized text. Feedback acknowledges processing with
`feedbackResponses: [{isProcessed: true}]`; the resulting pin state is checked
separately. Playlist deletion is verified through a fresh editable-playlist
listing, and opening a deleted or unavailable playlist returns an error.

Playlist rows come from `musicPlaylistShelfRenderer`, with occurrence identity
at `musicResponsiveListItemRenderer.playlistItemData.playlistSetVideoId`.
Editors retain duplicates, exclude recommendation shelves, and load playlist
continuations. Writable playlist pages expose
`musicEditablePlaylistDetailHeaderRenderer`; picker choices use
`contents[].addToPlaylistRenderer.playlists[].playlistAddToOptionRenderer`.

These features do not add a staticfs playlist-editing interface. Its existing
playback and queue files continue to use daemon requests.

## Live validation

Validated with the saved keyring session, external Chrome, Python probes, and
the locally built Rust client on 2026-09-05:

- Empty create/read, two song additions, moves in both directions, batch removal,
  and permanent deletion all passed with fresh reads of the saved state.
- Deletion without confirmation preserved the playlist; opening a deleted ID
  returned an unavailable-playlist error.
- Creating from a freshly generated 50-song radio preserved its contents and
  order after the creation visibility delay.
- A 125-song playlist loaded in full, in order, across continuation pages.
- Pinning was confirmed after bounded read polling, including from a fresh
  client. Song state matched the Listen again card instead of stale `/next` data.
- Unpin returned processed acknowledgements and success toasts but failed to
  change persisted state, including in Chrome. Static reported this failure.

Temporary live probes were removed from the source. Automated validation:
`cargo test --offline --features staticfs -- --test-threads=1`, 341 passed,
5 existing tests ignored. Formatting and whitespace checks passed.
