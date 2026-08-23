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
`user.getRecentTracks` at its documented 200-row limit; Last.fm's page total is
the oldest page, so Retune moves toward page 1. The metadata probe remains sequential; once the total is known,
source work launches bounded windows of four page requests and processes their
results in descending page order. It skips now-playing and undated rows, writes parsed raw pages
under a snapshot-specific machine cache, discards rows at or after `historyTo`
before caching or counting, acknowledges each page in a manifest only after
the atomic page write, and enforces 100 MiB session/cache ceilings. The exact
Last.fm username is recorded in both manifest and page metadata. The manifest
and session cursor remain a contiguous descending suffix: a window failure
keeps only its already-checkpointed prefix and restarts from the failed cursor,
so an exit refetches at most the unfinished part of one four-page window. No
aggregation or Spotify request occurs during source work. Once every manifest
page is present, raw-page reads, sorting, and aggregation run off the async
runtime; the importer then atomically enters review, or Done when no rows remain,
before best-effort cache cleanup.

The sequential metadata probe keeps its existing retry helper. Concurrent page
workers make one logical request after Last.fm's internal capped retry, parse
and classify the result without mutating importer state, and return it to the
ordered coordinator. The coordinator persists one retry attempt for the failed
cursor, waits at the capped delay, and retries that cursor in-process while the
app remains running; a failed request never advances its cursor. An
acknowledged missing, corrupt, oversized, or metadata-mismatched
page quarantines the snapshot and starts a fresh V2 session. An unacknowledged
page file is an ignorable orphan and can be overwritten on retry. V1 caches are
quarantined because the fixed cutoff changes page boundaries. Relaunching with
a saved Downloading or Aggregating session resumes it once after hydration;
an empty state never creates a session implicitly.

Opening a visible review batch lazily searches through the shared Spotify
client/request gate, serializes duplicate batch requests, binds the Spotify
account on the first match, and caches the results. Review batches are
persisted as stable `ImportBatch` pages capped at 100 source rows; large
artist/album groups, including singles, are split without changing the
artist-level cascade. Commands validate the page and artist/album identity; row
actions also validate a source ID, while album-level actions can address split
batches without one. Fuzzy disclosures stay inside the visible batch while
count modes remain target-wide.
Revisiting a cached batch does not call Spotify and there is no adjacent
prefetch. Accept All is the explicit bulk exception: it sequentially prepares
every remaining batch, shows global unique album/track URI counts, then applies
only after confirmation.

The Sum/Use highest/Zero choice is one reusable account-bound default applied to
every unlocked fuzzy target and restored for later import sessions. Accepting a
target freezes the strategy used for that target, while fuzzy disclosures remain
bounded to the visible persisted batch. The target-wide count decision still
includes selected, completed source rows from other batches. “Show Spotify search
terms” is likewise one persisted session
preference, restored when the importer resumes rather than copied into each
page’s options.

Whole-album acceptance sends one album URI to Spotify and updates
`SavedAlbumRecord`; selected-track acceptance sends only track URIs and updates
`saved_tracks`. Accept & Next first builds a frozen, account/session-bound apply
plan and atomically enqueues it in `lastfm-sync.json`; the command returns after
that enqueue, so the accepted batch disappears from the active projection while
the next batch can open immediately. One serial Rust worker then performs
upstream membership, local materialization/history/metadata, reusable mappings,
and review decisions in order, checkpointing its stage and removing the job only
after every effect succeeds. Replaying a stage is idempotent; failures retain a
retryable job and its frozen choices, while running jobs resume after restart.
Whole-album jobs never issue saved-track membership writes. Accept All stores one
compact cursor and creates only its next job. Explicit Spotify membership remains
on the Rust runtime through the shared request gate; completion/failure events
refresh the queue without replacing a newer selection. The source session is
bound to Last.fm first; Spotify
`/me` is nullable until the first lazy match. A later Spotify mismatch
suspends Spotify-derived work without invalidating the source snapshot.
The importer serializes each session mutation through durable replacement before
updating memory, and matching rechecks the expected account and phase at every
durable checkpoint and before entering review. Cached Spotify-derived pages
trust only an exact cached library identity; otherwise the current `/me` account
is resolved. Post-search ownership validation and match persistence hold the
shared Spotify membership gate together. Suspended state exposes no prior
account identity or queue. Source-phase Downloading/Aggregating state remains
Spotify-free; bound Review/Done reads and bound Suspended resume validate exact
Spotify ownership under the shared gate, while an unbound source suspension
requires only Last.fm. After Last.fm hydration, the Tauri shell—not React—
claims and starts one persisted Downloading/Aggregating runner using its stored
username and cutoff. React only observes progress and offers explicit
first-start/manual-resume actions.

Incremental reconciliation is separate from the historical baseline. The first
activation records `syncedThrough=now`; later launches, reconnects, and the
explicit Preferences action download only the fixed half-open range after that
checkpoint. The query is padded for Last.fm's exclusive `from`/`to` semantics,
then locally filtered to the exact range. It reuses the same parser, 200-row
pages, four-page bounded downloader, raw cache, manifest, retry limits, and
oldest-to-newest checkpointing, and performs no Spotify calls during source
download or aggregation. Reconciliation matches accepted local-scrobble
receipts as a multiset, applies mapped events additively with the latest
`last_played_at`, and leaves unknown or unavailable targets in a durable,
resumable review backlog. Accepted mappings and permanent track/album/artist
ignore rules sweep applicable backlog occurrences; Skip remains temporary.
There is no periodic timer. Exact application writes a before/after library
journal, backlog/checkpoint/receipt effects, and then atomically commits the
library boundary; recovery accepts only the recorded before or after state and
reports a typed conflict otherwise. Mappings are portable only; checkpoints,
receipts, active downloads, journals, and pending review remain machine-local.

## Cross-cutting rules

- Provider URI is the normal deduplication identity; local files use canonical
  `file://` URIs. Spotify ingestion also collapses alternate track URIs when the
  album slot and provider metadata match exactly.
- Album identity is source + overlay artist text + overlay album text.
- Overlay metadata is local-only. Remote content operations are explicit.
- Spotify HTTP traffic uses one shared client/request gate and typed cooldowns.
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
- [Spotify](docs/architecture/spotify.md)
- [Playback](docs/architecture/playback.md)
- [Persistence](docs/architecture/persistence.md)
- [Development](docs/DEVELOPMENT.md)
