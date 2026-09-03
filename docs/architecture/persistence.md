# Persistence

The Tauri shell stores Retune state in the platform application-data directory.
All JSON state writes use one shared primitive that creates a unique,
same-directory temporary file without truncating an existing writer, syncs it,
and atomically renames it into place. Windows serializes the final replacement
because simultaneous `MoveFileExW` calls can reject one another. Open, write,
sync, and rename failures
leave the previous destination intact and remove only that writer's temporary
file. Secret temporaries are created with mode 0600 on Unix before any bytes are
written; legacy secret permissions are repaired through the open descriptor
before their contents are read.

## Files

| File | Contents |
| --- | --- |
| `library.json` | Versioned core library and overlay |
| `settings.json` | UI, sync, and playback preferences |
| `playlists.json` | Playlist metadata/content cache |
| `cooldowns.json` | Typed Spotify endpoint cooldowns |
| `artist-genres.json` | Persistent Spotify artist-genre cache |
| `spotify-catalog.json` | Versioned machine-local Spotify artist, album, and track catalog; excluded from backup |
| `spotify-library.json` | Account-scoped exact Spotify saved-track and saved-album membership |
| `spotify-sync-journal.json` | Recoverable membership/library/settings transaction for a successful Spotify sync |
| `tokens.enc` | Encrypted release OAuth token state |
| `dev-tokens.json` | Development token state; mode 0600 on Unix |
| `dev-lastfm-session.json` | Development Last.fm session; mode 0600 on Unix |
| `lastfm-pending-token.json` | Short-lived Last.fm authorization token; mode 0600 on Unix |
| `lastfm-scrobbles.json` | V2 ordered pending queue plus accepted local-scrobble receipts; excluded from backup |
| `lastfm-import.json` | Versioned, account-bound Last.fm snapshot/review session; excluded from backup |
| `lastfm-import-cache/` | Disposable V2 parsed-page cache and authoritative manifests; excluded from backup |
| `lastfm-mappings.json` | Account-bound reusable track/album mappings and permanent ignore rules; optional in backup |
| `lastfm-sync.json` | Machine-local incremental checkpoint, active range/cache, backlog/journal, and the account/session-bound review-apply queue; excluded from backup |
| `lastfm-review-transaction.json` | Temporary redo journal for session/mapping review changes and cross-file Spotify account-ID migration; excluded from backup |
| `restore-journal.json` | Mode-0600 recoverable multi-file backup replacement journal; removed after completion |

Reads are bounded before JSON allocation. Settings, credentials, tokens, and
cooldowns are limited to 1 MiB; artist genres to 32 MiB; Last.fm raw cache,
session, and mappings files to 100 MiB; playlists, Spotify membership, catalog,
incremental Last.fm state, and the Last.fm review transaction to 256 MiB; the
library to 512 MiB; and the
restore journal to 1 GiB. Portable backup input is limited independently to
128 MiB compressed/plain input and 512 MiB after gzip expansion. Oversized data
follows the format's malformed/unsupported rejection or quarantine policy and
is never silently overwritten with defaults.
The same 1 GiB journal ceiling is enforced before the Applying marker is
written, so Retune either records a journal it can recover or performs no
restore writes.

Missing files use each format's documented empty/default state. Corrupt or
unsupported library, token, Spotify catalog/membership, and Last.fm
session/mapping/sync files are quarantined when their owner has a safe recovery
state, with the reset or reconnect condition surfaced to the user. Settings,
playlist, cooldown, artist-genre, backup, and restore-journal failures are
reported to their caller or recovery coordinator instead of being silently
discarded. Quarantine renames the evidence to a unique sibling and never writes
a replacement over it during the failing load.

`cooldowns.json` and `artist-genres.json` have separate concrete filesystem
owners. Full Spotify sync receives both; content actions and importer policy
receive only cooldown persistence. Their filenames and JSON formats are
unchanged.

`settings.json` retains `next_spotify_sync` as a serde-defaulted private Unix
deadline. It is written with the rest of settings but is omitted from
`SettingsView` and `SettingsPatch`, so the frontend cannot accidentally edit the
automatic schedule. Completed and partial Spotify attempts record the next
24-hour deadline; a connected fatal attempt records the same fallback before its
error is returned. An active persisted cooldown overrides it; an empty cooldown
leaves the daily value in place, and expired deadlines are cleared on
observation. Disconnect and sign-out do not clear this scheduling state.

`cooldowns.json` stores transient rate limits by endpoint family and one global
Development Mode quota record under `__global_quota__`. Reads remove expired
records and legacy per-endpoint quota records are coalesced to the latest global
deadline atomically on load. An active global quota dominates transient family
records; without it, callers use the relevant or earliest transient deadline.
Callers use that effective persisted deadline for sync gating, Last.fm retry
projections, and the Spotify status snapshot. Network-backed Spotify search
clears only the global quota record, while a persistent catalog hit leaves all
cooldown records intact.

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

Backup export clones the library, settings, and playlist cache under one owner
lock at a time, releasing each before acquiring the next, then fetches Last.fm
mappings and performs serialization and atomic replacement off the async
executor. A failed export preserves an existing destination. Backup replacement
validates every included component before writing. A versioned journal records the
exact before and after value for each included component before the first data
file changes. Startup examines an Applying journal before ordinary loads: every
current value must equal its recorded before or after value, otherwise recovery
reports a conflict and writes nothing; a valid mixture rolls forward to all
after values in the same order. The journal is atomically marked Complete before
best-effort cleanup, so a leftover completed journal is cleanup-only and cannot
overwrite later user changes. Missing optional backup components are untouched,
merge remains library-only, and user-visible change events are delayed until
all replacement files and the Complete marker are durable. Settings, playlist,
and library refreshes are then attempted independently, so one shell-side
notification failure cannot suppress the others or reclassify the durable
restore as failed.

`backup.rs` owns the portable envelope, native file dialogs, and multi-owner
runtime coordination. `restore.rs` remains the low-level journal and recovery
mechanism used by both startup and runtime replacement.

If a runtime replacement fails after the Applying journal is durable, Retune
immediately rolls the journal forward while the library, settings, playlist,
and Last.fm mapping owners are still exclusively held, then reconciles their
live values from the journal without writing them again. If that roll-forward
also fails, a shared in-process latch rejects later mutations of those four
owners before they can persist a third value; restarting performs the existing
startup recovery before constructing fresh mutation owners.

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
Its narrow filesystem store is composed with the in-memory state and async
mutation gate by the Spotify membership owner; cooldown and artist-genre
persistence remain separately owned.

A successful complete or partial Spotify sync writes a mode-0600
`spotify-sync-journal.json` containing exact before/after membership, library,
and settings values before changing any of those files. Each current value must
match its recorded before or after value during recovery; a valid mixture rolls
forward to all-after before normal startup loads. Runtime holds the membership,
library-transaction, and settings gates until the journal is complete, then
publishes all three live snapshots together. Caller cancellation cannot cancel
that owned commit. If immediate recovery cannot restore coherence, the shared
mutation latch rejects later writes until restart recovery succeeds.

`spotify-catalog.json` is a versioned `SpotifyCatalog` V1 wrapper containing
deterministic artist-ID, album-URI, and track-URI maps plus exact search-result
identity keyed by normalized request and immutable Spotify account ID. Search
pages reference catalog entities instead of duplicating their payloads. Saved
membership, ratings, plays, and Last.fm decisions remain elsewhere.
Unknown versus known-empty collections and entity/relationship completeness are
explicit; only validated Spotify IDs and URIs form identity. The shell constructs
the shared client with an empty catalog and loads the persisted catalog on an
owned blocking hydration task. The loaded snapshot installs only if the live
catalog generation is still the startup baseline; observations made while disk
parsing runs therefore win. Hydration installation and periodic/exit flushes
share the catalog flush gate, and successful installation publishes a Library
invalidation so catalog-backed projections refresh. Load failures are reported
without flushing the empty startup value over the source file. Dirty generations
are written with atomic replacement at most every 30 seconds and at exit, and
corrupt or unsupported files are quarantined. It is excluded from backup and has
no expiry. Disconnect and OAuth grant replacement retain it; `/me` selects the
search namespace belonging to the authenticated account.

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
For a non-empty release-shaped batch, the presence of its map entry is also the
durable opt-in marker for collection matching. Activation seeds the currently
selected release from session-cached match data, clears only album-level
selection and whole-album state, and preserves accepted mappings and explicit
track choices before reranking. The source album remains the command key; the
collection projection therefore survives restart with its selected albums and
mode intact. Converted batches reject whole-album options and apply plans.
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
entry is committed atomically before best-effort cache cleanup, whose failures
are logged without misreporting committed work. Account reset or replacement
deletes the old account cache before committing the new sync identity and
propagates cleanup failures. Session and cache
files use mode 0600 on Unix and a 100 MiB safety ceiling. Corrupt or unknown
session versions are quarantined and never applied. This machine/account state
is deliberately outside normal backup/restore, like `spotify-library.json` and
the scrobble queue.

`lastfm-scrobbles.json` is `ScrobbleLedgerV2`: the legacy array migrates to
`pending`, while successful code-0 submissions add corrected metadata, submitted
metadata, and timestamp receipts. Reconciliation consumes receipts and remote
events as multisets; ignored or rejected submissions do not create receipts.
Receipts are pruned only after the corresponding reconciliation commit.

The connector is composed immediately in an explicit Loading state, then an
owned startup task hydrates its credential, pending-token, and ledger stores on
the blocking pool before atomically publishing Ready. Commands project the
loading state and reject mutations until that publication; hydration completion
emits the ordinary connector state and starts deferred importer work. This
deliberately supersedes the older SOLID audit's single-phase-construction wording:
native credential stores may block, and TAURI-016 requires the main window and
unrelated startup work to remain responsive while they load. The stable
`lastfm::Service` facade owns that lifecycle; `lastfm/api.rs` owns signed request
execution, `lastfm/store.rs` owns credential and ledger persistence, and
`lastfm/listening.rs` owns generation-scoped eligibility and receipt decoding.
Fake request executors replace only transport, so authentication, scrobble, HTTP
status, and JSON tests exercise the same parameter/signature/response path as
release requests.

`lastfm-mappings.json` is account-bound and stores explicit source-track to
Spotify-track mappings, source-album mappings with normalized target track
names, the reusable count-merge default, and permanent excluded-track,
ignored-album, and ignored-artist rules.
Explicit track mappings win over album mappings. Skip decisions are not stored
there. Completed V2 historical sessions idempotently backfill accepted choices.
Unreadable or unsupported mappings are quarantined with a timestamped sibling
before fresh mappings are used, and the reset is reported in sync status.
Review actions and default count-mode changes that update both the import
session and reusable mappings first write `lastfm-review-transaction.json`,
then replace both files and remove the journal. Spotify account-ID migration
uses the same redo record for the session, incremental state, and mappings.
Startup and the next serialized mutation roll any surviving record forward
before accepting a third value. Disk work runs on the blocking pool, and an
owned completion publishes all corresponding in-memory snapshots together even
if the initiating command is cancelled.

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
Journaled library replacement and snapshot cleanup execute on the blocking pool;
the library transaction remains owned until the atomic write and memory
publication finish, so cancellation leaves a recoverable before-or-after state.

`lastfmScrobblingProfile` is persisted in settings and is accepted only when
its trimmed username is non-empty and `startedAt` is positive. Settings load,
save, and export restore all validate this boundary.

One native settings owner serializes every runtime mutation from the latest
in-memory value through normalization, validation, atomic replacement, and the
memory swap. User-visible changes then emit one `settings-changed` event in the
same serialized operation; private bookkeeping changes remain eventless.
Once a settings save starts, an owned completion retains the mutation gate
through the durable write and memory swap even if the invoking command is
cancelled.
Frontend commands send field patches or playback intents instead of persisted
snapshots. The public settings view omits
`spotifySyncCompleted`, `lastFullSync`, and `lastfmScrobblingProfile`; those
remain native bookkeeping in the unchanged `settings.json` format. Secondary
windows read the narrow appearance payload and subscribe to
`appearance-changed` rather than loading the settings record.
The existing lowercase `playbackBackend` and `repeat` bytes are closed enums;
missing values default to `local` and `off`, while unknown or case-changed
values are rejected at deserialization. Repeat remains a live playback intent,
not a generic settings patch.

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
not migrated; installations that still have only that retired format must
authenticate again. An in-process cache avoids repeated credential prompts. The
keyring mock backend is not enabled for release targets.

Debug builds and local bundles built with the `dev-token-store` feature use the
development token file. On Unix, Retune creates and checks that file with mode
0600. A legacy file with broader permissions is repaired through its open file
descriptor before any credential bytes are read; failure to establish mode 0600
aborts the load. Ordinary release bundles never enable that feature and retain the
encrypted-file/native credential-store boundary.

Missing token files mean disconnected. Malformed development JSON, truncated
or invalid encrypted data, and ciphertext that cannot authenticate with the
current credential-store key are one typed corruption condition, distinct from
ordinary filesystem or keyring failure. Startup quarantines that token file to
a timestamped `.corrupt-*` sibling, starts disconnected, and presents a
reconnect notice without overwriting the evidence. Backing-file and in-process
cache changes are serialized together; refresh responses commit only while the
grant they started from remains current.

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

Atomic replacement uses `rename` on Unix-like platforms. Windows uses one
reviewed `MoveFileExW` FFI call with replace-existing and write-through flags;
the unsafe boundary receives only live, nul-terminated UTF-16 path buffers.

## Change guidance

Persist only state that must survive relaunch. Keep caches reconstructible, use
atomic replacement for new state files, and define migration/default behavior in
tests whenever a serialized shape changes.
