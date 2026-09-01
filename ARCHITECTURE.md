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
   ├── retune-spotify    OAuth, Web API, normalization, materialized catalog
   ├── retune-audio      local-file scan, tags, decoding
   ├── playback          controller + Spotify/file backends
   ├── lastfm_import     account-bound history, incremental reconciliation, review, and apply boundary
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
- `retune-spotify` owns the install-local versioned Spotify music catalog;
  the desktop shell loads and atomically persists it, while account changes
  clear it before a new grant is exposed.
- The playback controller owns the canonical queue, active order, generation,
  and user-facing playback state.
- React owns view selection, navigation, dialog state, and transient gestures.
- `lastfm_import` owns the resumable Last.fm snapshot and incremental ranges,
  raw-page cache, compact source variants, reusable account-bound mappings,
  lazy Spotify matching results, review decisions, and application journal.
  Its parsing and review helpers do not add network or filesystem concerns to
  `retune-core`.

## Principal flows

### Library sync

The shell asks the shared Spotify provider for one complete library snapshot.
Progressive batches update a sync-owned working copy; only a successful run is
persisted and swapped into live state. Core upsert refreshes provider fields
while preserving local overlay edits. Responses also accrete complete and
partial artist, album, and track facts in the shared catalog.

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

The shell owns historical import and incremental reconciliation behind one
`lastfm_import` facade; `retune-core` remains deterministic and Last.fm-free.
The connector owns authentication, API requests, scrobble receipts, and its
queue, while the importer depends inward on those connector models. The Tauri
composition root coordinates account changes and incremental scheduling.
Inside the connector facade, focused `api`, `store`, and `listening` modules own
signed request execution, credential/ledger persistence, and playback-fact
policy respectively. The facade remains available in a loading state while an
owned task hydrates blocking native stores, preserving responsive startup.

Source snapshots, review state, cached Spotify candidates, durable apply jobs,
and recovery journals remain account-bound and machine-local. Explicit accepted
mappings are the portable exception. Spotify matching and membership writes use
the shared provider/request boundaries, and source download remains independent
of Spotify. Current source, review, application, and incremental contracts live
in [Library](docs/architecture/library.md); candidate ranking and ambiguity
policy live in [Last.fm import matching](docs/architecture/lastfm-import-matching.md).

## Cross-cutting rules

- Provider URI is the normal deduplication identity; local files use canonical
  `file://` URIs. Spotify ingestion also collapses alternate track URIs when the
  album slot and provider metadata match exactly.
- Album identity is source + overlay artist text + overlay album text.
- Overlay metadata is local-only. Remote content operations are explicit.
- Spotify Web API traffic uses one shared client/request gate and typed
  cooldowns. OAuth token requests use the same low-level transport outside that
  gate.
- Files containing application state use atomic replacement.
- Incremental Last.fm application is journaled before the atomic library write;
  local play changes cannot interleave with that authoritative boundary.
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
- [Last.fm import matching](docs/architecture/lastfm-import-matching.md)
- [Spotify](docs/architecture/spotify.md)
- [Playback](docs/architecture/playback.md)
- [Persistence](docs/architecture/persistence.md)
- [Development](docs/DEVELOPMENT.md)
