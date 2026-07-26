# Views & Library Polish workstream

Scope: hideable browser panes, configurable/sortable tracklist columns with
new metadata columns (plays, kind, bit rate, last played), play tracking,
sidebar playlist reordering + unfollow/delete, and a single-prompt token
store. All view preferences ride through export/import library.

Facts (validated against the tree at b53f762):

1. Tracklist column visibility (right-click checkboxes) and drag-reorder
   already exist (`App.tsx` TrackList, `Settings.columnOrder` /
   `hiddenColumns`), and already export/import via the `VisualSettings`
   envelope in `export_with_settings` / `import_with_settings` (lib.rs).
   There is NO header-click sorting today.
2. Track ordering comes from `retune_core::browse::tracks` (Rust). The
   frontend receives a pre-ordered array. Sorting will be implemented
   frontend-side so browse order remains the "no sort" baseline.
3. `TrackRecord` (crates/retune-core/src/model.rs) has no play_count,
   last_played_at, kind, or bitrate. `AudioInfo` (retune-audio) exposes
   sample_rate/channels/duration; average bitrate for local files is
   file_size*8/duration — duration is already persisted, so backfill only
   needs `fs::metadata`.
4. Natural track completion is observable in the playback layer: reducer
   turns `EndOfTrack` into `Advance` (or `Reload` on repeat-one) and
   `ConnectBoundary` marks a finished remote run. Manual next/prev must NOT
   count as plays.
5. Tokens live in ONE keychain item behind `CachedTokenStore` (read once
   per launch), but `save()` writes through on every token refresh. With
   ad-hoc signing each keychain access can prompt; the fix is to keep only
   a random key in the keychain and store tokens in an AEAD-encrypted file.
6. The View menu (lib.rs `install_file_menu`) already has a CheckMenuItem
   pattern (zebra striping) and an `on_menu_event` → frontend-event bridge.

## Decisions

- **Browser pane visibility**: right-click a pane (header or body) offers
  "Hide Genre/Artist/Album". View ▸ Column Browser submenu holds three
  check items plus "Show Browser" (Cmd+B) which toggles the whole strip.
  With all panes hidden the strip collapses to a full-height tracklist;
  the View menu (always reachable) is the canonical way back — that
  answers the "how do you restore it" problem without any magic zones.
  Hiding a pane clears that facet's selection (no invisible filtering).
- **Kind strings** (iTunes-style): "Spotify", "MPEG audio file",
  "AAC audio file", "Apple Lossless audio file", "FLAC audio file",
  "WAV audio file", "AIFF audio file", "Ogg Vorbis audio file",
  "Opus audio file", "WebM audio file". Derived at import from
  probe/extension; Spotify tracks are constant.
- **Bit rate**: local files only, average kbps computed at import
  (size*8/duration/1000, rounded); Spotify shows blank. Existing local
  imports are backfilled on library load (cheap: fs::metadata + stored
  duration; kind from extension).
- **Play semantics**: play_count increments and last_played_at is set when
  a track finishes naturally (EndOfTrack → Advance/Reload, incl. repeat-one
  and ConnectBoundary-completed tracks). Skips don't count. Applies to
  Spotify and local tracks alike; stored in the overlay, so it exports.
- **Sorting**: click a header to sort asc, click again for desc. Indicator
  arrow in the header. Tie-break cascade after the clicked column:
  track_no → artist → album → genre (skipping the clicked column if it's
  in the cascade; case-insensitive text, numeric for numbers, None last).
  "No sort" = browse order; sorting by "#" ascending approximates it.
  Sort state persists in Settings and exports. Double-click-to-play uses
  the *displayed* (sorted) order for the queue.
- **Default columns**: ON in order Name, Artist, Album, #, Time, Rating,
  Genre, Plays. OFF by default: Kind, Bit Rate, Last Played. Name can
  never be hidden. Saved column orders missing the new keys get them
  appended; a saved order equal to the old default is upgraded to the new
  default.
- **Playlist order**: user drag-order lives in the playlist overlay store
  (not Spotify), survives sync (known ids keep their position; new
  playlists append), and round-trips through export/import library.
- **Playlist unfollow/delete**: both map to Spotify
  `DELETE /v1/playlists/{id}/followers` (Spotify models delete-own as
  unfollow). Context menu on a sidebar playlist: "Delete Playlist…" when
  owned, "Unfollow Playlist…" otherwise, with a confirm step. Upstream
  call first; local removal only on success (atomic, same guardrail
  philosophy as write-back).
- **Token store**: release builds move to `EncryptedFsTokenStore` — a
  random 32-byte key in the keychain (created once, read once per launch)
  + AEAD-encrypted token file in app_data_dir. Token refresh saves touch
  only the file, so worst case is ONE keychain prompt per launch (zero
  once signing is stable). One-time migration from the legacy keychain
  item, which is then deleted. Dev builds keep FsTokenStore.

## Tickets (TDD, sequential)

### T0 — Bug: followed playlists never populate
`playlists::sync` gives non-owned playlists `summary_only` (name + count,
no tracks) — the detail view renders 141 tracks as an empty list. Fix:
fetch track contents for followed playlists exactly like owned ones
(snapshot-id caching already avoids refetch); mutation paths keep their
existing `owned` guards. Accept: fake-transport test syncing a followed
playlist yields its tracks; unchanged snapshot skips the fetch.

### T1 — Browser pane visibility
Settings field (e.g. `browser_panes`), View ▸ Column Browser submenu with
three CheckMenuItems + Show Browser (CmdOrCtrl+B), right-click context menu
on panes, grid collapses to N visible columns / zero-height strip, hiding
clears that facet selection, VisualSettings export/import round-trip.
Accept: hide all three → full-height tracklist; View menu restores; prefs
survive export→wipe→import.

### T2 — Track metadata model
TrackRecord gains `play_count: u32` (serde default), `last_played_at:
Option<u64>`, `kind: Option<String>`, `bitrate_kbps: Option<u32>`.
Local import fills kind+bitrate; Spotify sync fills kind. Startup backfill
for existing local/spotify records missing kind/bitrate (no probing —
extension + fs::metadata only), saved once. TrackView exposes all four.
Accept: import a fixture → kind/bitrate populated; old library JSON loads
with defaults and backfills.

### T3 — Play tracking
Completion hook in the playback layer emits track-finished (uri) on
EndOfTrack-driven Advance/Reload and ConnectBoundary; lib.rs updates
overlay (play_count += 1, last_played_at = now), saves, emits
library-changed. Skip via next/prev does not fire. Accept: reducer-level
tests for count/no-count cases; library update path unit-tested.

### T4 — New columns + sorting
Add Plays/Kind/Bit Rate/Last Played columns (existing toggle/reorder
machinery), new defaults + saved-order migration, header-click sort with
cascade + indicator, sort state in Settings + VisualSettings, queue
follows displayed order. Name column not hideable (menu item disabled).
Accept: sort by Name groups dupes by track_no then artist/album/genre;
toggling/reordering/sort survive export/import.

### T5 — Playlist reorder + unfollow/delete
Sidebar drag to reorder playlists (persisted order in playlist store,
sync-stable, in export envelope). Context menu Delete/Unfollow with
confirm; DELETE /playlists/{id}/followers upstream, local removal on
success only; errors surface through the existing error dispatch.
Accept: order round-trips export/import and survives a sync; delete of
owned + unfollow of followed both hit the correct endpoint (fake
transport) and never mutate locally on failure.

### T6 — Single-prompt token store
`EncryptedFsTokenStore` in retune-spotify (or store.rs): keychain holds a
random key (one read per launch), tokens in AEAD-encrypted file; saves
never touch the keychain. Migration: legacy keychain tokens imported on
first run, legacy item deleted. Startup audit: exactly one keychain read
(debug log seam). AEAD crate: prefer one already in the lock file, else
chacha20poly1305. Accept: unit tests with fake keychain (key reuse,
migration, corrupt file → treated as absent); release path constructs the
new store.

## QA fix-up round (2026-07-26)

Findings from the first manual pass, sequenced as F-tickets:

- F1 Context menus: clamp to viewport (no off-window overflow), open at
  the cursor for ALL column headers (Plays/Kind/Last Played headers
  currently don't open the menu; Rating opened it at the window edge),
  playlist menu opens far from the click.
- F2 Sidebar: mouse-wheel and arrow-key scrolling don't work (window
  must be resized to reveal playlists); playlist drag-reorder does
  nothing in practice — debug and fix.
- F3 Followed playlists STILL empty after rebuild: cache entries
  predating T0 have matching snapshot_ids with empty tracks, and the
  snapshot short-circuit trusts them forever → refetch when
  track_count > 0 but tracks is empty. Also: playlist track names now
  render bold in owned playlists — find and fix the regression.
- F4 Date Added: `added_at` (unix seconds, UTC) on TrackRecord; Spotify
  saved-tracks `added_at` when the API provides it, import time for
  local files, first-sync/now placeholder backfill for existing records.
  "Date Added" column, default OFF, date-only display, full-timestamp
  sort precision; exports like the rest.
- F5 Finder drag-and-drop import: dropping files/folders onto the window
  routes through the existing import pipeline (dedupe, summary, events).
  Sequenced after F4 so imports stamp added_at.
- F6 Play threshold: Preferences setting "count as played after N%"
  (default 100% = current completion behavior). Counting/last-played
  fire on a single internal TrackPlayed signal at threshold crossing
  (or completion if never crossed); skips before threshold don't count.
- F7 Column width resize: draggable dividers on variable-width columns
  (fixed ones like Rating excluded), persisted + exported.
- F8 Universal pane menu: right-clicking ANY browser pane header opens
  one checkbox menu listing Genre/Artist/Album (exactly the tracklist
  column-menu style — extract a reusable checkbox-menu component and use
  it for both), replacing the single "Hide X" item.

Parked pending user mockups (do NOT build yet): sidebar playlists
chevron collapse/expand + New Playlist moved to the bottom; browser-pane
visibility moving from the View menu to Preferences ▸ Appearance; other
Preferences visual tweaks. A change-manifest.md from the user is
incoming and will define that batch.

## Done means

Panes hide/show from menu and right-click with sane empty state; tracklist
has 10 columns with iTunes sort behavior and the specified defaults;
finishing a song bumps Plays and Last Played; playlists drag-reorder and
delete/unfollow sync upstream; export→import restores every view pref;
app startup asks for the keychain at most once.

Deferred: per-column widths, sort within browser panes, play-count
threshold semantics (e.g. iTunes' "counts at end" is what we implement;
no partial-play rules), Smart Playlists.
