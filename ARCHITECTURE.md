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

- The desktop shell currently targets macOS.
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
