# Persistence

The Tauri shell stores Retune state in the platform application-data directory.
All JSON state writes use a temporary file followed by atomic rename.

## Files

| File | Contents |
| --- | --- |
| `library.json` | Versioned core library and overlay |
| `settings.json` | UI, sync, and playback preferences |
| `playlists.json` | Playlist metadata/content cache |
| `cooldowns.json` | Typed Spotify endpoint cooldowns |
| `artist-genres.json` | Persistent Spotify artist-genre cache |
| `spotify-library.json` | Account-scoped exact Spotify saved-track and saved-album membership |
| `tokens.enc` | Encrypted release OAuth token state |
| `dev-tokens.json` | Development token state; mode 0600 on Unix |
| `dev-lastfm-session.json` | Development Last.fm session; mode 0600 on Unix |
| `lastfm-pending-token.json` | Short-lived Last.fm authorization token; mode 0600 on Unix |
| `lastfm-scrobbles.json` | V2 ordered pending queue plus accepted local-scrobble receipts; excluded from backup |
| `lastfm-import.json` | Versioned, account-bound Last.fm snapshot/review session; excluded from backup |
| `lastfm-import-cache/` | Disposable V2 parsed-page cache and authoritative manifests; excluded from backup |
| `lastfm-mappings.json` | Account-bound reusable track/album mappings and permanent ignore rules; optional in backup |
| `lastfm-sync.json` | Machine-local incremental checkpoint, active range/cache, backlog/journal, and the account/session-bound review-apply queue; excluded from backup |

The official Tauri window-state plugin manages the main native window's size,
position, and maximized state in machine-local application state. Its lifecycle
handles restoring and saving this state; it is not part of backup/export.

The Preferences Bug tab reads the current rotating application log directly.
A startup marker limits the viewer to the current process session. View filters
do not change the report window: Copy Logs and Email include session entries
through the final warning or error and omit trailing informational entries.
Email support is compiled from optional `RETUNE_SUPPORT_EMAIL`; missing local
configuration disables only Email, and the frontend never receives the address.

The token record has an optional reusable built-in playback credential containing
the librespot username and AP authentication bytes. Its absence is the default,
so older token files remain readable. Release builds keep it inside encrypted
`tokens.enc`; on Unix, development token files retain the existing mode-0600
boundary.
Refreshing the Web API token preserves the playback credential. Playback
rejection removes only this field, while explicit Spotify disconnect removes the
whole token record. Replacing the Web OAuth grant clears the playback credential
because the new grant may belong to a different account. It is machine-specific
and never belongs in backup/export.

Built-in Spotify playback also maintains an `audio-cache` directory. Cache data
is disposable; library and settings files are not.

The playlist cache retains Spotify display metadata for every fetched track,
including disc/track numbers and album release date. Older caches deserialize
with defaults and are refreshed once before snapshot-based fetch skipping resumes.

`spotify-library.json` is separate from `library.json` and is not included in
portable backup or restore. Its minimal shape is `SpotifyLibraryState`:
`account_id`, `complete`, `saved_tracks` (track URI to optional `added_at`), and
`saved_albums` (album URI to `SavedAlbumRecord`). Each saved album record stores
`uri`, `name`, `artists`, `release_date`, `album_type`, `added_at`, and
`track_uris`; artwork and popularity are deliberately omitted. It is written
with the same temporary-file-and-rename atomic replacement as other app data.
Missing or incomplete state is unknown and does not authorize destructive
reconciliation until a complete sync establishes the exact account state.

`lastfm-import.json` is `LastFmImportSessionV2`. It stores the immutable
`historyTo`, an optional `downloadedThrough` timestamp, Last.fm username,
nullable Spotify `/me` account ID, snapshot cache ID, descending page cursor,
downloaded/total pages, totals, retryable error and attempt, session defaults
for the two independent intents (content and historical play counts) plus
whole-album mode, compact aggregated rows, decisions, stable 1-based
`ImportBatch` pages capped at 100 source rows, batch options, match
results/candidates/selected URIs, a
reusable default plus frozen Spotify-target-to-Sum/Overwrite/Zero map, and the
session-level search-term display preference. Match candidates include the
backward-compatible serde-defaulted `inLibrary` projection (`false` for older
sessions); it is cache-derived membership UI state, not a new acceptance or
mapping decision. Last.fm pages are written atomically as parsed
raw-page files; the manifest is written only after the page file and is the
authority for recovery. The manifest and every page record the exact Last.fm
username as well as cutoff/page metadata, so punctuation-distinct accounts
cannot share a snapshot. Unacknowledged orphan files are ignored/overwritten.
Collection review additionally stores the serde-defaulted, batch-keyed
`collectionAlbumMatches` map: cached Spotify album previews, selected album
URIs in selection order, and serde-defaulted per-source provenance for only the
candidate URIs injected by a selected-album rerank. Coverage, per-track match
status, and whole-album readiness are derived projections and are not persisted.
Preview/add/remove mutations use the same serialized read-modify-write and
atomic replacement path; legacy V2 sessions load an empty collection map and
treat the new provenance as empty, retaining the existing release-shaped
behavior. This session state is excluded from backup/export along with the rest
of `lastfm-import.json`.
`downloadedThrough` is serde-defaulted for older V2 JSON and remains absent
until a valid checkpointed timestamp is available; loading an older session does
not scan cached pages to backfill it. It advances monotonically with ordered
page checkpoints and is exposed alongside `historyTo` to the importer UI.
An acknowledged missing, corrupt, oversized, or metadata-mismatched page
quarantines the entire snapshot and starts a fresh V2 session. The sequential
metadata probe retains its existing retry helper. Concurrent page workers make
one logical request after Last.fm's capped internal retry is exhausted and
return structured outcomes without mutating importer state; the ordered
coordinator persists one retry attempt and waits at the capped delay before
re-entering from the failed cursor without advancing it. No raw page is aggregated until the manifest is complete; review
entry and cache cleanup follow one atomic session write. Session and cache
files use mode 0600 on Unix and a 100 MiB safety ceiling. Corrupt or unknown
session versions are quarantined and never applied. This machine/account state
is deliberately outside normal backup/restore, like `spotify-library.json` and
the scrobble queue.

`lastfm-scrobbles.json` is `ScrobbleLedgerV2`: the legacy array migrates to
`pending`, while successful code-0 submissions add corrected metadata, submitted
metadata, and timestamp receipts. Reconciliation consumes receipts and remote
events as multisets; ignored or rejected submissions do not create receipts.
Receipts are pruned only after the corresponding reconciliation commit.

`lastfm-mappings.json` is account-bound and stores explicit source-track to
Spotify-track mappings, source-album mappings with normalized target track
names, the reusable count-merge default, and permanent excluded-track,
ignored-album, and ignored-artist rules.
Explicit track mappings win over album mappings. Skip decisions are not stored
there. Completed V2 historical sessions idempotently backfill accepted choices.
Unreadable or unsupported mappings are quarantined with a timestamped sibling
before fresh mappings are used, and the reset is reported in sync status.

`lastfm-sync.json` stores the Last.fm/Spotify identities, `syncedThrough`,
`lastSyncedAt`, one fixed padded download range and cache identity, stable
unresolved backlog, sync error, and the before/after application journal. The
first activation records `syncedThrough=now` and does not backfill. A range is
locally filtered to `[syncedThrough, cutoff)` and is not applied until every
page is cached. The journal records exact affected-library values plus
checkpoint, backlog, and receipt effects before `library.json` changes. Recovery
finalizes only an exact before or after match and reports a typed conflict for
anything else. Disconnect or Last.fm account replacement clears this machine
sync state and receipts but preserves owner-bound mappings; Spotify identity
mismatches suspend safe application. Unreadable or unsupported sync state is
quarantined with a timestamped sibling, then reset to a fresh no-checkpoint
state so the next sync starts at its current activation time.

`lastfmScrobblingProfile` is persisted in settings and is accepted only when
its trimmed username is non-empty and `startedAt` is positive. Settings load,
save, and export restore all validate this boundary.

`settings.json` carries the exportable optional
`lastfmScrobblingProfile` (`username`, `startedAt`). Missing legacy profiles
are backfilled on the first successful enable/import; the same username keeps
its cutoff across toggles, while a different username replaces it. A successful
live validation keeps the completed V2 session, profile, and recovery backup;
the backup is restored only when validation fails or rollback is required.
Every session read-modify-write is serialized from its in-memory snapshot
through JSON serialization, blocking atomic replacement, and the in-memory
swap. Raw-page writes and aggregation are kept off the async runtime, and the
session cursor is rechecked after a page write before acknowledgement.
Suspended account-bound reads are redacted rather than exposing the previous
owner.

Column layout is UI state in `settings.json`: the Library has one order, width map,
and hidden-column list. Playlists have independent metadata-column order, width,
and visibility overrides keyed by Spotify playlist ID; absent playlist keys mean
the default layout (fixed Spotify order `#`, Song, Artist, Album, Time, Rating,
Plays, Genre). The legacy `playlistHiddenColumns` map remains readable and
portable. Restoring a playlist aspect to its default removes that playlist's
override instead of storing redundant defaults.

## Spotify audio cache

The audio cache contains complete encrypted Spotify audio files keyed by
Spotify `FileId`. A cache miss downloads ranges into a sparse temporary file;
only a completed download is copied into `audio-cache`. Interrupted and abandoned
downloads therefore do not become persistent cache entries.

The cache has a fixed 2 GiB limit. When a completed file pushes it over the
limit, the least-recently-used entries are removed until it fits. Hits update the
in-process access order. After relaunch, that order is reconstructed from
filesystem access time, with modification or creation time as fallbacks, so
eviction is LRU-like rather than a durable exact playback history.

Cache identity is independent of Retune's library and search projections. Both
reuse the same entry when Spotify resolves them to the same `FileId`; a different
audio format or quality may resolve to a different file. The cache is disposable
and excluded from backup and restore.

## OAuth token security

Release builds encrypt `tokens.enc` with authenticated encryption. A random file
key is stored in the platform-native credential store under service
`com.rianjs.retune` and account `token-file-key`: macOS Keychain, Windows
Credential Manager, or Linux Secret Service. A legacy native token entry is
migrated into the encrypted file and then removed. An in-process cache avoids
repeated credential prompts. The keyring mock backend is not enabled for release
targets.

Debug builds and local bundles built with the `dev-token-store` feature use the
development token file. On Unix, Retune creates and checks that file with mode
0600. Ordinary release bundles never enable that feature and retain the
encrypted-file/native credential-store boundary.

Last.fm release session keys use the native credential store with service
`com.rianjs.retune` and account `lastfm-session`; the username is stored beside
that credential value and is never sent to the frontend except as connected
account display state. Debug builds and local bundles use
`dev-lastfm-session.json`. Authorization request tokens use the short-lived
owner-only pending-token file. Pending scrobbles and accepted receipts are
written atomically oldest-first to the V2 ledger, sent in batches of at most 50,
and pending items are removed after an
accepted/ignored response (including ignored code 3). The queue is
machine/account state and is intentionally omitted from backup and restore;
each queued scrobble carries its non-secret owning Last.fm username. Session and
app-identity failures preserve it for reconnect, while permanent request
rejections remove and log the affected batch. Reconnecting as the same username
preserves and drains the queue; a different username clears it durably before
installing the new session. Ownerless or mixed legacy queues are never flushed
and are cleared during account reconciliation. Disconnect clears the durable
queue before clearing session or pending authorization state; a failed queue
clear leaves the active account connected.

Last.fm runtime mutations snapshot queue/account state under the persistence
serialization mutex, perform filesystem and credential-store work in blocking
tasks, and commit only when the snapshot is still current. This keeps queue
ordering and account isolation intact without holding the async runtime mutex
across local I/O; failed queue writes or clears retain the corresponding
in-memory state.

## Recovery and portability

A corrupt library file is quarantined with a timestamped `.corrupt-*` suffix;
Retune starts with an empty library and reports the problem rather than
overwriting the evidence.

Backup/export produces JSON or gzip containing the core library plus portable
settings, playlist cache, and optional `lastfmMappings`. Restore validates and
replaces the library, restores mappings dormant until their Last.fm and Spotify
identities match, and applies exported portable settings/playlists after
confirmation. Merge is additive library-only: it ignores exported settings,
playlists, and mappings, deduplicates by URI, and preserves existing overlay
values. Checkpoints, receipts, active downloads, journals, and pending review
are never exported.

Machine-specific credentials and Spotify client configuration are not portable
backup data.

## Change guidance

Persist only state that must survive relaunch. Keep caches reconstructible, use
atomic replacement for new state files, and define migration/default behavior in
tests whenever a serialized shape changes.
