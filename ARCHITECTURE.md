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
   ├── lastfm_import     account-bound snapshot, matching, review, and apply boundary
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
- `lastfm_import` owns the resumable Last.fm snapshot, compact source variants,
  Spotify matching results, review decisions, and account-bound application.
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
1320×840. The importer captures one fixed Last.fm `to` timestamp, fetches
`user.getRecentTracks` sequentially at 200 rows per page, skips now-playing and
undated rows, and atomically checkpoints compact aggregate variants plus the
next page. Its session defaults independently control content and historical
play counts (both on by default, with whole-album content mode off); at least
one remains selected. Matching then runs sequentially through the shared
Spotify client; album candidates are limited to ten and classified from real
track-set overlap without monopolizing the membership gate.

Fuzzy count strategies are persisted once per Spotify track target for the
session, and the review page discloses every source row in the session that
resolves to that target. “Show Spotify search terms” is likewise one persisted
session preference, restored when the importer resumes rather than copied into
each page’s options.

Whole-album acceptance sends one album URI to Spotify and updates
`SavedAlbumRecord`; selected-track acceptance sends only track URIs and updates
`saved_tracks`. Upstream membership completes before the atomic local history
and metadata mutation, and a durable decision is marked done only after that
mutation succeeds. The session is bound to both the Last.fm username and
Spotify `/me` account ID; a mismatch suspends it.

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
