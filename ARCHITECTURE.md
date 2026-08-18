# Retune architecture

Retune is a single-process Tauri desktop application. React renders views and
invokes Tauri commands. The Rust application shell composes a pure library
domain, Spotify integration, local-audio support, persistence, and playback.

```text
React UI
   │ Tauri commands/events
   ▼
desktop application shell
   ├── retune-core       pure library and browse model
   ├── retune-spotify    OAuth, Web API, normalization
   ├── retune-audio      local-file scan, tags, decoding
   ├── playback          controller + Spotify/file backends
   ├── lastfm_import     account-bound snapshot, lazy matching, review, and apply boundary
   └── store             app-data persistence
```

The composition root is `run()` in
`apps/desktop/src-tauri/src/lib.rs`; `main.rs` only calls it. The application
shell owns orchestration and persistence. Domain crates do not depend on Tauri.

## Package map

| Path | Responsibility |
| --- | --- |
| `crates/retune-core` | Tracks, overlay edits, ratings, browse projections, merge/restore, portable serialization |
| `crates/retune-spotify` | PKCE authentication, token storage interfaces, Web API client, retries/rate limits, response normalization |
| `crates/retune-audio` | Local-file discovery, metadata/artwork probing, decoding, and seeking |
| `apps/desktop/src-tauri` | Commands, menus, sync orchestration, stores, playlists, playback, media keys, app lifecycle |
| `apps/desktop/src` | React views and ephemeral interaction state |

## State ownership

- The core `Library` owns durable overlay records and pure projections.
- The Tauri shell owns live application state and saves changes through stores.
- Spotify owns remote library membership, follows, and owned-playlist content.
- The playback controller owns the canonical queue, active order, generation,
  and user-facing playback state.
- React owns view selection, navigation, dialog state, and transient gestures.
- `lastfm_import` owns the resumable Last.fm snapshot, raw-page cache, compact
  source variants, lazy Spotify matching results, review decisions, and
  account-bound application.
  Its parsing and review helpers do not add network or filesystem concerns to
  `retune-core`.

## Principal flows

### Library sync

The shell asks the shared Spotify provider for each library section in sequence.
Batches are applied incrementally so partial progress is useful. Core upsert
refreshes provider fields while preserving local overlay edits. The resulting
library is persisted atomically.

### User mutation

React invokes a Tauri command. The shell validates the operation, mutates a
locked in-memory model, persists it, then emits an event for affected views.
Remote content actions complete against Spotify before local canonical caches are
updated.

### Playback

The controller selects the local-file engine or configured Spotify backend for
the current URI. Backends emit neutral events; one reducer rejects stale events,
updates state, advances the queue, and records threshold-based play counts.

### Last.fm import

The Preferences action opens a second `lastfm-importer` WebviewWindow at
1320×840. An account-bound `lastfmScrobblingProfile` records the first
successful connection/enable timestamp for each Last.fm username; toggling
preserves the same username's timestamp and a different username replaces it.
The importer captures that fixed `historyTo`, probes metadata once, and fetches
`user.getRecentTracks` at its documented 200-row limit from the oldest page
toward page 1. It skips now-playing and undated rows, writes parsed raw pages
under a snapshot-specific machine cache, discards rows at or after `historyTo`
before caching or counting, acknowledges each page in a manifest only after
the atomic page write, and enforces 100 MiB session/cache ceilings. The exact
Last.fm username is recorded in both manifest and page metadata. No
aggregation or Spotify request occurs during source work. Once every manifest
page is present, raw-page reads, sorting, and aggregation run off the async
runtime; the importer then atomically enters review, or Done when no rows remain,
before best-effort cache cleanup.

The source runner persists retry state after Last.fm's internal capped retry is
exhausted, waits at the capped delay, and retries the same probe/page in
process while the app remains running; a failed request never advances its
cursor. An acknowledged missing, corrupt, oversized, or metadata-mismatched
page quarantines the snapshot and starts a fresh V2 session. An unacknowledged
page file is an ignorable orphan and can be overwritten on retry. V1 caches are
quarantined because the fixed cutoff changes page boundaries. Relaunching with
a saved Downloading or Aggregating session resumes it once after hydration;
an empty state never creates a session implicitly.

Opening a visible review batch lazily searches through the shared Spotify
client/request gate, serializes duplicate batch requests, binds the Spotify
account on the first match, and caches the results. Revisiting a cached batch
does not call Spotify and there is no adjacent prefetch. Accept All is the
explicit bulk exception: it sequentially prepares every remaining batch,
shows global unique album/track URI counts, then applies only after
confirmation.

Fuzzy count strategies are persisted once per Spotify track target for the
session, and the review page discloses every source row in the session that
resolves to that target. “Show Spotify search terms” is likewise one persisted
session preference, restored when the importer resumes rather than copied into
each page’s options.

Whole-album acceptance sends one album URI to Spotify and updates
`SavedAlbumRecord`; selected-track acceptance sends only track URIs and updates
`saved_tracks`. Upstream membership completes before the atomic local history
and metadata mutation, and a durable decision is marked done only after that
mutation succeeds. The source session is bound to Last.fm first; Spotify
`/me` is nullable until the first lazy match. A later Spotify mismatch
suspends Spotify-derived work without invalidating the source snapshot.
The importer serializes each session mutation through durable replacement before
updating memory, and matching rechecks the expected account and phase at every
durable checkpoint and before entering review. Cached Spotify-derived pages
trust only an exact cached library identity; otherwise the current `/me` account
is resolved. Post-search ownership validation and match persistence hold the
shared Spotify membership gate together. Suspended state exposes no prior
account identity or queue.

## Cross-cutting rules

- Provider URI is the normal deduplication identity; local files use canonical
  `file://` URIs. Spotify ingestion also collapses alternate track URIs when the
  album slot and provider metadata match exactly.
- Album identity is source + overlay artist text + overlay album text.
- Overlay metadata is local-only. Remote content operations are explicit.
- Spotify HTTP traffic uses one shared client/request gate and typed cooldowns.
- Files containing application state use atomic replacement.
- Local playback must not require Spotify authentication.

## Current constraints

- The desktop shell targets macOS arm64, Windows x64/ARM64, and Ubuntu 22.04
  amd64/arm64.
- Spotify Premium is required for built-in Spotify playback.
- Spotify restricts third-party access to playlists the current user does not
  own; Retune does not request those items.
- Audiobook chapters can be synchronized but are not currently playable.

## Domain details

- [Library](docs/architecture/library.md)
- [Spotify](docs/architecture/spotify.md)
- [Playback](docs/architecture/playback.md)
- [Persistence](docs/architecture/persistence.md)
- [Development](docs/DEVELOPMENT.md)
