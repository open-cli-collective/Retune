# Rust alignment audit completion record

Status: completed and independently re-audited, 2026-08-31. All findings
`RUST-001` through `RUST-047` are resolved in the authoritative tree.

This document is retained as the user-requested exhaustive audit record. The
item bodies below preserve the original pre-remediation evidence, proposed
changes, and proof criteria; they are not current architecture or an active
work queue. Current boundaries and invariants live in
[ARCHITECTURE.md](../ARCHITECTURE.md) and the owning documents under
[docs/architecture](architecture/). Current verification commands live in
[DEVELOPMENT.md](DEVELOPMENT.md).

Final independent verification passed all 846 workspace tests with one ignored
real-device test, formatting, strict workspace Clippy, documentation checks,
104 frontend tests, warning-free frontend lint, the production frontend build,
release-contract and Tauri ACL checks, release-mode tests, and native package
construction.

## Original executive verdict

Retune has good bones: `retune-core` is deterministic and isolated, the
playback reducer owns visible state, Spotify traffic uses one shared client and
request gate, state writes generally use replacement rather than in-place
mutation, and native release coverage is unusually broad. The only first-party
`unsafe` is the small Windows `MoveFileExW` boundary required for replace-existing
atomic writes; its pointers are live nul-terminated UTF-16 buffers. Formatting,
Clippy, and the complete local test suite pass.

The important debt is concentrated rather than pervasive:

- one persisted Last.fm identifier can escape its cache root and reach recursive
  deletion;
- several clone/await/save flows are not linearizable and can lose concurrent
  settings, playlist, or token changes;
- several filesystem, gzip, and HTTP boundaries allocate before applying a
  ceiling;
- corrupt audio can be reported as a successful end-of-track;
- current Spotify account, scope, pagination, and retry contracts are not fully
  represented;
- bulk library operations have credible quadratic shapes; and
- `lastfm_import.rs` now owns enough independent behavior that invariants are
  materially nonlocal.

The list below is intentionally pragmatic. It does not ask for a database,
blanket abstraction, lint churn, zero-copy rewrites, or new concurrency/testing
frameworks. Most repairs use patterns or standard-library types already present
in the repository.

## Original baseline established

| Area | Audit result |
| --- | --- |
| Workspace | Four crates; approximately 51,900 lines of Rust including tests |
| Local toolchain | `rustc`/Cargo 1.98.0 |
| Declared toolchain | Desktop claims 1.85; other members do not declare a Rust version |
| Formatting | `cargo fmt --all --check` passed |
| Lints | `cargo clippy --workspace --all-targets -- -D warnings` passed |
| Tests | Full workspace rerun passed: 601 passed, 1 intentionally ignored real-device test |
| Unsafe | One Windows-only `MoveFileExW` FFI boundary for atomic replacement |
| CI targets | macOS arm64, Windows x64/ARM64, Ubuntu 22.04 amd64/arm64 release tests/builds |
| External verification | Current official Spotify contracts checked on 2026-08-30 |
| Not exercised | Live Spotify/Last.fm accounts, real audio output, and manual native UI journeys |

Passing this baseline is meaningful, but it does not exercise malformed
persisted state, overlapping commands, lost responses after remote writes,
oversized inputs, or the current live Spotify contract. Those are where most of
the work lies.

## Historical priority and severity

Priority is implementation order; severity uses the supplied reviewer scale.

| Priority | Meaning |
| --- | --- |
| P0 | Contain before doing unrelated feature work; credible destructive data-loss path |
| P1 | Next correctness/reliability wave |
| P2 | High-value hardening, performance, or maintainability work after P1 |
| P3 | Small cleanup or documentation correction; safe to combine with nearby work |
| Measure | Do not change yet; collect the named evidence first |

Historical recommended order:

1. Contain `RUST-001` immediately.
2. Establish safe persistence and mutation ownership with `RUST-002` through
   `RUST-013`.
3. Repair trust boundaries, playback, and current Spotify behavior with
   `RUST-014` through `RUST-034`.
4. Fix algorithm shape and cheap dependency/build debt with `RUST-035` through
   `RUST-045`.
5. Split modules only after the owning invariants are fixed, using `RUST-046` as
   a mechanical final wave.

## P0: contain destructive persisted paths

### [x] RUST-001 — Constrain Last.fm cache identities before any path operation

**P0 · blocking · R-C1, R-API1, R-T1**

Evidence:

- `LastFmImportSessionV2.cache_id` and `IncrementalRange.cache_id` are arbitrary
  persisted `String`s at `apps/desktop/src-tauri/src/lastfm_import.rs:347-356`
  and `:2806-2817`.
- load validation checks versions and cursor/options but not path shape at
  `lastfm_import.rs:2998-3023` and `:3122-3159`.
- `cache_path` is `self.cache_root.join(cache_id)` at `:3172-3174`.
- that result reaches `create_dir_all`, reads/writes, `remove_dir_all`, and
  `rename`, including `remove_snapshot` at `:3393-3395`.

An absolute ID replaces `cache_root`; `../…` escapes it; an empty ID targets the
cache root itself. A tampered or corrupt app-state file can therefore delete or
rename unrelated user data. This meets the review standard's blocking threshold
for credible irreversible data loss.

Minimum repair:

- represent a cache ID as one non-empty `Component::Normal`, or validate that
  exact property at every deserialize boundary;
- where identity inputs are available, recompute `snapshot_cache_id` or
  `incremental_cache_id` and require equality rather than trusting the stored
  value;
- make destructive helpers independently reject absolute, parent, root, empty,
  and symlink targets and propagate deletion errors; and
- quarantine the bad state without touching the referenced snapshot.

Do not solve this with string prefix checks or `canonicalize` alone: missing
targets and sibling prefixes make those incomplete. A component check plus an
independent destructive-operation guard is smaller and safer.

Proof: create sibling sentinel directories, load both session forms with an
absolute ID, `../sentinel`, `.`, empty text, and a symlink component, then drive
load, clear, invalidate, quarantine, and aggregation paths. Every sentinel must
survive and invalid state must be rejected/quarantined.

Owning documentation: `docs/architecture/lastfm-import-matching.md` and
`docs/architecture/persistence.md`.

## P1: persistence and concurrent state ownership

### [x] RUST-002 — Make `Library` invariants intrinsic to deserialization

**P1 · major · R-C1, R-E1, R-API1**

`Library` publicly derives `Deserialize` at
`crates/retune-core/src/model.rs:119-125`, while duplicate ID/URI rejection and
`next_id` repair live in `validate_imported` at `:358-381`. Only
`retune_core::io::import` calls it. Last.fm journals embed `Library` directly at
`lastfm_import.rs:2927-2932`, deserialize through `lastfm-sync.json`, and can
install `after_library` during recovery.

That makes invariant repair optional by call path. Parseable corruption can
restore duplicate identities or a hostile `next_id` and later make lookup
ambiguous or make `fresh_id` panic.

Minimum repair: deserialize through a private wire struct, reject duplicate
track IDs, duplicate/empty URIs, and duplicate album-rating keys, recompute
`next_id`, then construct `Library`. The core version-envelope check remains in
`io`; the model invariants do not. Do not add a ceremonial `ValidatedLibrary`
wrapper that callers can bypass.

Proof: directly deserialize valid and invalid journal libraries, including
duplicate album keys and `u64::MAX`; invalid state must be rejected before
recovery, while the handwritten v1 fixture and normal journal round trips remain
valid.

### [x] RUST-003 — Use collision-proof atomic replacement for shared stores

**P1 · major · R-C1, R-A1, R-T1**

`apps/desktop/src-tauri/src/store.rs:859-875` always opens the same
`*.json.tmp` with `create(true).truncate(true)`. Concurrent writers can truncate
or interleave the same inode and race its rename, defeating the documented
atomic-file invariant.

Reuse the already-correct shape in `lastfm.rs:298-324` and
`retune-spotify/src/tokens.rs:273-293`: a unique sibling name, `create_new`, full
write, `sync_all`, rename, and best-effort cleanup. Keep secret-mode handling a
separate option or helper; non-secret callers do not need security policy.

This prevents torn bytes only. It does not solve stale snapshot lost updates;
the aggregate gates below are still required.

Proof: barrier-synchronize different-length writes to the same file over many
iterations. The final file must always equal one complete parseable input and no
temp remains. Inject open/write/sync/rename failures and retain the previous
document.

### [x] RUST-004 — Create development token files as `0600` before writing

**P1 · major · R-C1 and the token-persistence invariant**

`FsTokenStore::save` writes through the generic temp and applies `0600` only
after rename at `store.rs:828-847`. With a permissive umask, the temporary
plaintext access, refresh, and playback credentials can be readable by other
users; a post-rename chmod failure leaves the final file exposed.

Use the Last.fm secret-writer pattern: randomized `create_new` temp and Unix
mode `0600` at creation. On load, repair or reject an insecure legacy mode.
Release encrypted-token storage already does this correctly and should not be
rewritten.

Proof: pause an injected write and inspect both temp and final modes; simulate a
post-write failure; neither file may ever be group/world-readable.

### [x] RUST-005 — Give settings one linearizable mutation/commit boundary

**P1 · major · R-C1, R-A1, R-T1**

`set_settings` clones settings, awaits other services, then persists and replaces
the whole snapshot at `apps/desktop/src-tauri/src/lib.rs:755-801`. Volume,
repeat, shuffle, audio, Last.fm, auto-connect, sync bookkeeping, and restore each
have sibling clone/save/replace paths. A field update completed during another
command's await can be silently overwritten.

Some commands also apply live effects before persistence, while others persist
and later return an error from a secondary menu/playback effect. The caller
cannot reliably tell whether the preference committed.

Minimum repair:

- add one settings mutation gate/helper;
- obtain unrelated async inputs before the gate where possible;
- under the gate, re-read the latest settings, apply only the command's fields,
  normalize/validate, persist, then publish the in-memory value;
- run filesystem work off async executor threads; and
- perform fallible secondary effects after commit with an outcome that does not
  pretend the preference rolled back.

Do not make `retune-core` async or create a generic transaction framework.

Proof: hold full settings save at an await while changing volume, shuffle, and
audio. Disk and memory must contain all changes. Inject persistence and
post-commit effect failures and assert the reported commit state.

### [x] RUST-006 — Serialize playlist sync and mutation transactions

**P1 · major · R-C1, R-A1**

Create, add, reorder, remove, and unfollow clone the complete cache, await
Spotify, then save/replace it (`playlist_commands.rs:51-62`, `:200-223`,
`:249-307`, and `lib.rs:1832-1868`). Playlist sync follows the same shape at
`lib.rs:1439-1462`. Two remote operations can both succeed while last-writer-wins
local persistence erases one result.

Add one async playlist-operation gate around snapshot → remote operation →
persist → publish, including full playlist sync. Continue using the short
`std::sync::Mutex` only to clone/publish; do not hold it across `.await`.

Remote and local state cannot be atomically committed together. If a remote
write succeeds but local persistence fails, mark the cache incomplete and force
a reload before presenting or retrying it rather than leaving the old snapshot
authoritative.

Proof: delay two fake-provider mutations and a sync; final disk/memory must
contain every committed remote result, including stale-snapshot reload and local
save-failure paths.

### [x] RUST-007 — Serialize every token mutation with an epoch/CAS check

**P1 · major · R-C1, R-A1**

Refresh holds only a client's private `refresh_lock`, awaits the token endpoint,
then unconditionally saves values derived from the old grant
(`retune-spotify/src/client.rs:952-987`). Connect, playback-credential update,
and disconnect mutate the same shared store through unrelated gates
(`spotify_commands.rs:121-137`, `:198-216`, `:250-261`).

Consequences include an old refresh overwriting a new OAuth grant, refresh
resurrecting tokens after disconnect, and refresh/playback load-modify-save
losing one another. `CachedTokenStore` can also publish disk/cache steps from
overlapping mutations out of order.

Put one mutation epoch or conditional-update primitive at the shared token-store
boundary. Refresh captures the grant/epoch before its await and commits only if
unchanged; grant, clear, and playback updates use the same boundary. One
implementation is enough—do not add a token service hierarchy.

Proof: block an old refresh, disconnect or install a new grant, then release it;
final cache and disk must stay cleared/new. Concurrent playback-credential update
and refresh must preserve both fields.

### [x] RUST-008 — Snapshot backup state without lock inversion or executor blocking

**P1 · major · R-A1, R-C1**

Backup export holds `library → settings → playlists` together at
`lib.rs:2293-2304`. Playlist projection holds `playlists → library` at
`playlist_commands.rs:66-72`, creating a credible deadlock. The dialog callback
also uses `block_on`, serializes/compresses synchronously, and overwrites the
destination with `fs::write` at `lib.rs:2283-2305`.

Clone each aggregate under only its own lock, then release it. Fetch async
mappings asynchronously, perform serialization and file IO on a blocking task,
and write a synced unique sibling before replacing the destination. Preserve the
old backup on any failure.

Proof: barrier-run export snapshot and playlist projection under a timeout;
inject serialization/write/sync/rename failures and assert the prior backup
survives.

### [x] RUST-009 — Bound plain and gzip backup input before allocation

**P1 · major · R-C1, R-P1**

Restore reads the entire selected file at `lib.rs:2431-2441`, then expands gzip
with unbounded `read_to_end` at `:2380-2391`. A small gzip bomb or oversized
external file can exhaust process memory.

Choose and document a realistic maximum backup size. Check the compressed file
metadata, read through `Read::take(MAX + 1)`, and independently limit
decompressed output the same way before serde. The decompressed limit is the
important gzip-bomb defense.

Proof: exact-limit plain/gzip inputs pass; limit+1, sparse oversized files, and a
high-ratio gzip fail before a large allocation.

### [x] RUST-010 — Make multi-file restore resolve to all-old or all-new

**P1 · major · R-C1, R-T1**

`apply_import` commits library, then settings, playlists, and Last.fm mappings
with early returns at `lib.rs:2472-2510`. A later failure leaves a mixed durable
state and can omit change notifications.

Validate every component first, stage all bytes, then use one small restore
journal (or equivalent recoverable manifest) so startup completes or rolls back
the set. Reuse atomic replacement; do not build a general database transaction
layer. Preserve account-binding rules for portable Last.fm mappings.

Proof: inject failure/crash at every stage, restart, and assert the complete old
or complete new backup—not any mixture—is visible.

### [x] RUST-011 — Apply per-format ceilings before all persisted-file reads

**P1 · major · R-C1, R-P1**

Examples include Last.fm import session, incremental state, and mappings
(`lastfm_import.rs:2998-3056`, `:3122-3129`), general JSON stores
(`store.rs:635`, `:742-749`, `:758`, `:797`, `:830`), Last.fm ledger/session
state (`lastfm.rs:282-289`), and encrypted tokens
(`retune-spotify/src/tokens.rs:220-247`). The session's 100 MiB check happens
after `fs::read` has already allocated it.

Open the file, inspect metadata, and read through `take(limit + 1)`. Use
domain-specific limits: credentials/settings are tiny, caches and libraries are
not. Quarantine reconstructible cache/state; fail safely for credentials and
user library data. Do not impose one arbitrary universal ceiling.

Proof: exact-limit, limit+1, and sparse oversized fixtures for each policy class
must exercise the intended reject/quarantine behavior without proportional
allocation.

### [x] RUST-012 — Release runner flags with RAII on panic or cancellation

**P1 · major · R-A1, R-T1**

Spotify sync and Last.fm source/apply/sync runners set booleans with manual
`begin`/`finish` or claim/release pairs (`sync_orchestrator.rs:15-35`,
`lastfm_import.rs:3813-3819`, `:4142-4148`, `:5455-5461`). A panic or aborted
future can skip the release and wedge subsequent work until restart.

Use a tiny owning guard whose `Drop` clears the running claim. Keep explicit
normal-completion code only where rerun/coalescing semantics require it.

Proof: abort and panic a barrier-blocked runner, then assert a new request can
claim it and queued rerun semantics still work.

### [x] RUST-013 — Serialize cooldown read-modify-write updates

**P2 · minor · R-C1, R-A1**

`record_cooldown` reads the entire file, inserts one family, and saves it at
`lib.rs:1669-1685`, while sync keeps a separate in-memory map and persists from
`provider.rs:241-249`. Explicit actions and sync can drop each other's endpoint
deadline even after unique temp files fix byte integrity.

Give `FsSyncStore` one small locked `update_cooldowns` operation and route expiry
cleanup through it. This is a single-process aggregate; no file-locking crate is
needed.

Proof: concurrent distinct-family updates and expiry cleanup must preserve every
live deadline.

## P1: local files and playback boundaries

### [x] RUST-014 — Return partial scan results and path-specific failures

**P1 · major · R-C1, R-E1**

`retune-audio/src/import.rs:22-33` returns an empty vector after any recursive
error and silently drops canonicalization failures. `scan` materializes and
sorts each directory before the final `BTreeSet` sorts again. Desktop therefore
reports “successful, zero files” with no `ImportSummary.failed` entry at
`localfiles.rs:39-66`.

Return a small scan report containing accessible paths plus path-specific
failures, continue siblings, and feed failures into the existing summary. Iterate
`read_dir` directly; final canonical-set ordering is already deterministic.

Proof: a valid audio sibling plus an unreadable directory, dangling symlink, or
disappearing file imports the valid item and reports the bad path. Existing
symlink deduplication remains intact.

### [x] RUST-015 — Distinguish clean EOF, recoverable packets, and fatal decode

**P1 · major · R-C1, R-E1, R-T1**

`FileSource::decode_packet` converts every non-EOF demux/decoder error to
iterator end at `retune-audio/src/lib.rs:147-182`. The file backend interprets
an empty sink as `EndOfTrack` at `playback/file.rs:307-317`, so corrupt/truncated
audio can advance the queue and emit completion/listening facts as if playback
finished normally.

Follow Symphonia's error semantics: skip recoverable packet decode/IO errors,
keep looking for later samples, and expose a one-shot fatal status to the sink
owner. A sink drained after fatal failure emits request-scoped
`Unavailable`/error, never `EndOfTrack`.

Proof: a damaged middle packet still yields later samples; an injected terminal
failure emits no natural completion or completion fact; clean EOF is unchanged.

### [x] RUST-016 — Parse `OpusHead` at its specified position

**P2 · major · R-S1, R-C1**

`opus_channels` searches anywhere in codec-private bytes for the substring
`OpusHead` and reads a relative byte at `retune-audio/src/lib.rs:357-365`.
Garbage-prefixed data can therefore be accepted with parameters inconsistent
with the adapter's fixed-offset header reads.

This is a tiny binary grammar: require the magic prefix, minimum header length,
supported version, and valid channel count at the fixed byte. No parser
dependency is warranted.

Proof: table-test valid mono/stereo plus truncated, garbage-prefixed, bad-version,
zero-channel, and unsupported-channel headers.

### [x] RUST-017 — Prepare local imports before acquiring library locks

**P1 · major · R-A1, R-P1**

Import is correctly moved to `spawn_blocking`, but `run_local_import` enters
`with_library_gate`, which holds the write gate and live library mutex while the
closure recursively scans, probes/decodes, reads tags, saves, and commits
(`lib.rs:1919-1929`, `:2011-2044`; `localfiles.rs:29-80`). Large or slow folders
can block every browse and playback-metadata read for minutes.

Scan/probe into prepared `NewTrack` results before locking. Then acquire the
write gate, briefly clone the latest library, release its read mutex, dedupe/apply
and save, and briefly swap under the mutex. The write gate prevents lost writers
while readers retain the previous snapshot.

Proof: a blocked fake preparation must not block a library read; a concurrent
write must survive; only a completely persisted import becomes visible.

### [x] RUST-018 — Bound and offload local artwork work

**P1 · major · R-A1, R-P1, R-C1**

Async artwork resolution directly reads tags and base64-encodes on the executor
at `lib.rs:1020-1054`, then caches up to 512 values by count only at
`:1063-1067`. Separately, bulk import always clones the first embedded picture in
`retune-audio/src/lib.rs:279-289` even though `localfiles::map_file` discards it
and artwork is loaded later on demand.

Add a generous embedded-art byte ceiling at the audio boundary, run local
read/encode in `spawn_blocking`, and use a basic tag-read path that omits pictures
during import. Track cache bytes rather than only entry count if measurement
shows materially varied art sizes; the hard per-item ceiling comes first.

Proof: slow tag IO does not stop an async ticker, oversized art is rejected, bulk
import does not materialize pictures, and on-demand tagged artwork still works.

### [x] RUST-019 — Validate client-authored playback snapshots at IPC

**P1 · major · R-C1, R-API1**

`play_tracks` accepts an unbounded `Vec<SnapshotTrack>` directly from IPC at
`playback_commands.rs:4-15`. The client controls IDs, arbitrary file URIs,
metadata, duration, and queue size (`playback/mod.rs:148-157`), all of which feed
playback and Last.fm listening/scrobble facts.

Set a defensible queue and field-length cap, validate start index and URI kind,
require local file URIs to resolve to a current library record, and use canonical
server-side local metadata. Preserve the ability to play validated Spotify
search results that are not yet in the overlay.

Proof: reject oversized queues, malformed/wrong-kind URIs, unknown local files,
extreme durations/text, and mismatched local metadata before backend or
scrobbling work.

### [x] RUST-020 — Recheck playback intent after slow local activation

**P1 · major · R-C1, R-A1**

`switch_to_local_with` prepares a local backend outside the controller lock,
then installs it unconditionally at `playback/mod.rs:832-853`.
`switch_to_connect` can set `local_requested = false` and install Connect while
preparation awaits; the stale local completion can then stop/replace that newer
choice.

After preparation and after acquiring the controller lock, verify the latest
intent (or a small operation epoch) before committing. Tear down/discard a stale
prepared backend. A task/cancellation framework is unnecessary.

Proof: barrier-block local preparation, switch to Connect, release preparation,
and assert Connect remains active with the newer generation.

### [x] RUST-021 — Treat file-playback thread creation as recoverable

**P2 · minor · R-E1**

`FileEngine::new` panics if `thread::Builder::spawn` fails at
`playback/file.rs:51-62`. Resource exhaustion should make local-file playback
unavailable, not crash an otherwise usable Spotify/local-library application.

Return a disabled/fallible file engine and surface the existing unavailable or
startup-notice path. Do not make every playback constructor generic over a
spawner outside the focused test seam.

Proof: injected spawn failure leaves the app/controller usable and file playback
returns a distinguishable unavailable error.

## P1: current Spotify and network contracts

### [x] RUST-022 — Bind durable state to Spotify `account_id`, with migration

**P1 · major · R-C1, R-API1, R-T1**

`Profile` models only `id` and deprecated `product` at
`retune-spotify/src/client.rs:1103-1113`. `.me().id` currently binds persisted
Spotify membership/catalog reset and Last.fm import identity
(`lib.rs:1241-1265`; `lastfm_import.rs:5764-5781`).

Spotify's current [profile contract](https://developer.spotify.com/documentation/web-api/reference/get-current-users-profile)
and [May 2026 changelog](https://developer.spotify.com/documentation/web-api/references/changes/may-2026)
say `account_id` is immutable and intended for account linking; `id` is not.

Add `account_id` and use it for durable sync/import binding. Keep `id` for
playlist `owner.id` comparisons. Do not change the librespot username comparison
until a live/fake integration establishes which identity it returns.

Migrate without false account switches: if a legacy persisted value equals the
current profile `id`, rewrite it to that profile's `account_id`; clear only when
it matches neither. Apply the same compatibility to active Last.fm sessions.

Proof: decode differing IDs, migrate same-account legacy state without clearing,
reset a true mismatch, and retain playlist ownership behavior.

Owning documentation: `docs/architecture/spotify.md`,
`docs/architecture/lastfm-import-matching.md`, and persistence format notes.

### [x] RUST-023 — Request every scope the implemented endpoints require

**P1 · major · R-C1, R-API1**

`REQUIRED_SCOPES` at `retune-spotify/src/auth.rs:15-27` omits
`user-read-playback-position`, while sync calls Show Episodes and Saved Episodes
(`client.rs:477-489`; `provider.rs:869-914`). Current official contracts list
that scope for [Show Episodes](https://developer.spotify.com/documentation/web-api/reference/get-a-shows-episodes)
and [Saved Episodes](https://developer.spotify.com/documentation/web-api/reference/get-users-saved-episodes).

Add it so existing `Tokens::missing_scopes` correctly drives reauthorization.
At the same time, rederive the minimal set from actual endpoints: unified library
calls no longer need the old follow scopes, and `user-read-private` may become
unnecessary after `RUST-024`. Remove a scope only after its final consumer is
gone.

Proof: authorize URL/scope-set tests include all and only required scopes; an old
token reports the missing scope; manual signed-in episode sync succeeds after
reauthorization.

### [x] RUST-024 — Stop using deprecated `product` as a hard playback gate

**P1 · major contract hardening · R-C1, R-API1**

`Profile::is_premium` is exactly `product == "premium"`, and
`lib.rs:869-888` rejects built-in playback when the field is absent. Spotify's
[February 2026 Development Mode migration](https://developer.spotify.com/documentation/web-api/tutorials/february-2026-migration-guide)
lists `product` among removed user fields; the current profile reference marks
it deprecated. A valid Premium user can therefore be denied before Retune tries
the actual playback authorization/session.

Treat `product` as advisory at most and let the playback session establish
capability, mapping its concrete authorization failure. Retain a helpful Premium
message, but do not infer “not Premium” from a missing field.

Proof: a profile without `product` reaches playback authorization; actual
playback rejection still produces the expected user-facing Premium guidance.

### [x] RUST-025 — Never blindly replay ambiguous Spotify mutations after 5xx

**P1 · major · R-C1, R-A1**

`SpotifyClient::api_request` retries every method on 500/502/503/504 at
`client.rs:842-910`. That includes non-idempotent playlist creation and additive
item POSTs, relative/snapshot-bound reorder/removal, and time-sensitive playback
start/seek. The server may have committed before the failing response, so replay
can create duplicate playlists/items, convert success into a stale-snapshot
failure, or restart playback twice.

Automatically retry GET only. For mutations, return a typed “outcome unknown;
reconcile before retry” error and make the owning caller reload state before
offering another attempt. Add a narrow whitelist later only when an operation's
documented semantics prove replay safe.

Relevant contracts: [Create Playlist](https://developer.spotify.com/documentation/web-api/reference/create-playlist),
[Add Items](https://developer.spotify.com/documentation/web-api/reference/add-items-to-playlist),
[Update Playlist Items](https://developer.spotify.com/documentation/web-api/reference/reorder-or-replace-playlists-items),
and [Remove Playlist Items](https://developer.spotify.com/documentation/web-api/reference/remove-items-playlist).

Proof: fake `[500, success]` responses for create/add send exactly once and
return ambiguous outcome; GET retains bounded 1/3-second retries; callers
reconcile before another mutation.

Update the blanket 5xx statement in `docs/architecture/spotify.md`.

### [x] RUST-026 — Page through all nested episodes/chapters and propagate skips

**P1 · major · R-C1, R-T1**

Saved show expansion calls one `show_episodes(id, 0, 50)` and ignores `next` at
`provider.rs:884-898`; saved audiobooks do the same for chapters at `:931-945`.
Both official endpoints cap pages at 50: [Show Episodes](https://developer.spotify.com/documentation/web-api/reference/get-a-shows-episodes)
and [Audiobook Chapters](https://developer.spotify.com/documentation/web-api/reference/get-audiobook-chapters).
Several non-music paths also count `Page::skipped` for offsets but do not mark
the section partial.

Mirror the existing album-track offset loop: advance by decoded plus skipped,
stop on `next == None` or zero progress, aggregate every page, and surface any
skipped item as section-specific partial state. Keep sequential quota-conscious
fetching; do not add fan-out.

Proof: 51-item/two-page shows and books request the second offset and return all
items; malformed items advance safely and mark the relevant section incomplete.

### [x] RUST-027 — Validate semantic invariants of a persisted complete catalog

**P1 · major · R-C1, R-API1**

`SpotifyCatalog::is_supported` checks only version at
`retune-spotify/src/catalog.rs:54-57`, and `store.rs:634-667` accepts any
serde-valid supported version. Complete cached pages then trust map keys, URI
order, referenced tracks, totals, and fields (`catalog.rs:290-315`); `limit == 0`
can create a non-progressing page.

Add `SpotifyCatalog::validate()` at load: keys must match stored IDs/URIs,
complete lists must match totals and resolve every expected entity, required
complete fields must exist, and zero-limit cached paging must reject or miss.
Quarantine semantic corruption like invalid JSON.

Proof: handwritten mismatched keys, missing tracks, inconsistent totals,
complete stubs, and zero limits are rejected; valid old/current round trips load.

### [x] RUST-028 — Do not infer playlist absence from a partially decoded page

**P1 · major · R-C1**

`Page<T>` deliberately records malformed items in `skipped`
(`client.rs:1116-1146`). Playlist sync rebuilds from decoded summaries and drops
unmatched cached playlists at `playlists.rs:54-125` without treating skipped
summaries as incomplete. A response-shape drift can therefore delete a still
remote playlist from Retune's cache.

If any summary page skips items, retain unmatched prior entries or return a
typed incomplete-enumeration result and leave the immutable input cache intact.
Tolerant page decoding itself is useful and should remain.

Proof: one valid and one malformed summary preserves the malformed item's prior
cached entry and does not claim exact reconciliation.

### [x] RUST-029 — Bound HTTP bodies and token-exchange duration

**P1 · major · R-C1, R-A1, R-P1**

Spotify transport buffers `response.bytes()` without a ceiling at
`retune-spotify/src/client.rs:128-147`; OAuth exchange does the same at
`auth.rs:119-142`; Last.fm does so at `lastfm.rs:1561-1603`. Default OAuth
clients also have no request timeout. Network bytes remain untrusted even when a
logical API page has a documented item count.

At each owning transport, reject an excessive `Content-Length` early and collect
chunks with checked cumulative length so chunked responses cannot bypass the
cap. Pick generous domain-specific limits from the largest expected page plus
headroom. Bound/truncate error bodies under the same policy. Set the token
request timeout inside `auth::token_request` so all callers inherit it.

No streaming JSON framework or dependency is needed.

Proof: oversized fixed-length and chunked responses stop at the cap; exact-limit
JSON parses; a loopback token server that never responds times out predictably.

### [x] RUST-030 — Preserve typed Spotify failures through retry/UI boundaries

**P1 · major · R-S1, R-E1, R-API1**

The shared client already returns typed rate/quota errors, but desktop commonly
converts them to `String`. Last.fm apply later reconstructs endpoint/deadline by
matching exact English prefixes and splitting `;` at
`lastfm_import.rs:5464-5475`; the frontend also classifies prefixes. Explicit
Spotify removals bypass the action-cooldown helper at
`spotify_commands.rs:518-540` and `:713-716`.

Carry one small internal failure payload—display message, kind, endpoint/family,
retry time, and ambiguous-outcome flag—until the final IPC/log boundary. Route
all saves and removals through it. Preserve underlying error sources and a
bounded final server body where useful; do not create a huge error taxonomy.

Proof: change `Display` wording and retain cooldown/retry behavior; rate-limited
removals persist their deadline; quota without a supplied deadline invents none;
representative IO/transport sources remain discoverable.

### [x] RUST-031 — Validate successful token and follow responses semantically

**P2 · minor · R-C1, R-API1**

`TokenResponse` accepts empty access/refresh strings (`auth.rs:90-97`), and
refresh can replace a valid secret with `Some("")` at `client.rs:980-987`.
`is_following_artist` treats an empty or oversized boolean array as false at
`client.rs:601-609`.

Validate non-whitespace access tokens and non-empty optional refresh tokens once
at decode. Require exactly one boolean for a one-item contains request and let
the UI represent protocol failure/unknown rather than authoritative false.

Proof: malformed successful tokens never alter storage; `[]` and two booleans
fail, while exactly one value succeeds.

### [x] RUST-032 — Validate Spotify IDs before URL-path interpolation

**P1 · major · R-C1, R-S1, R-API1**

`provider::spotify_id` merely takes the suffix after the last colon at
`provider.rs:402-404`, and many client methods interpolate raw IDs into paths.
Wrong-kind URIs and IDs containing `/`, `?`, or `#` can select the wrong endpoint
or alter its query. The current [Spotify ID contract](https://developer.spotify.com/documentation/web-api/concepts/spotify-uris-ids)
defines base-62 identifiers, and stricter helpers already exist elsewhere.

Centralize a tiny expected-kind parser/validator and reuse it at IPC/client
boundaries; support bare IDs only where the UI contract needs them. No general
URI parser framework is warranted.

Proof: wrong kind, empty, extra-colon, slash/query/fragment, and non-base62 input
produce zero transport calls.

### [x] RUST-033 — Retry native key retrieval after transient failure

**P2 · minor · R-C1, R-A1**

`EncryptedFsTokenStore` caches `Result<[u8; 32], String>` in a `OnceLock` at
`retune-spotify/src/tokens.rs:185-216`. One transient credential-store failure is
therefore permanent until process restart.

Cache only a successful key. On a miss, make one ordinary key-source call and
install success; the next caller naturally retries a previous failure. Do not add
a retry loop/backoff policy here.

Proof: a source that fails once then succeeds recovers on the second call, while
a successful key is loaded only once.

### [x] RUST-034 — Use checked time conversion for persisted deadlines

**P3 · minor · R-C1, R-T1**

`provider::format_resume_time` casts persisted `u64` to `i64` with `as` at
`provider.rs:406-420`. Extreme corrupt state wraps to a bogus past time.

Use `i64::try_from` and the existing numeric fallback.

Proof: normal values, `i64::MAX`, and `u64::MAX` are deterministic and never
wrap.

## P1/P2: algorithm shape and blocking work

### [x] RUST-035 — Add transient indexes at bulk library boundaries

**P1 · major · R-P1, R-O1**

`Library::add`, `upsert`, `merge`, and album cleanup repeatedly scan the track
vector (`retune-core/src/model.rs:180-235`, `:321-354`). Spotify sync adds an
alternate-match scan and another upsert scan per incoming item
(`sync.rs:177-244`; `lib.rs:1968-1981`). Local import already proves uniqueness
with a `HashSet` and then scans again. Sync applies incremental batches and later
replays the complete track set at `lib.rs:1280-1286` and `:1370-1378`.

Use standard-library transient URI and composite-identity maps/sets once per bulk
operation, update them as records are added, optimize `merge`/`remove_uris`, and
avoid the redundant final replay. Keep the canonical `Vec` and stable insertion
order; `tracks_mut` makes a permanent second index expensive to keep correct.

Proof: preserve overlay/category/alias/dedup semantics and benchmark or count
comparisons for 10k/20k/50k sync, merge, and import inputs. Doubling should be
near-linear rather than approximately quadrupling.

### [x] RUST-036 — Index playlist projection for one call

**P2 · major at scale · R-P1**

For every playlist URI, `playlist_track_views` linearly searches cached Spotify
tracks and the whole library at `playlist_commands.rs:75-149`, while holding both
locks. Large playlists over large libraries are quadratic.

Build two temporary URI maps for the projection. Do not persist another index.

Proof: exact output parity and a 10k-playlist/50k-library benchmark or comparison
counter with near-linear behavior.

### [x] RUST-037 — Reuse browse results and compute sort keys once

**P2 · minor · R-P1**

Browse projection is computed, then `counts` repeats `browse::tracks` and several
library scans (`library_commands.rs:16-31`; `lib.rs:605-650`). Comparators also
allocate lowercase artist/album strings and rescan selection ranks on every
comparison (`retune-core/src/browse.rs:104-139`). This runs on search keystrokes.

Pass the already selected rows into count calculation, derive totals in one pass,
and decorate/cache lowercase/rank sort keys once per item (for example
`sort_by_cached_key`). Add no persistent search index until profiling still shows
a problem.

Proof: count `browse::tracks` invocations and benchmark the expected 10k–50k
library; preserve stable insertion and case-insensitive ordering.

### [x] RUST-038 — Flush artist genres once per sync, off the executor

**P1 · major at current cap · R-A1, R-P1**

Each uncached artist inserts then serializes, atomically writes, and fsyncs the
entire growing map while holding its mutex at `provider.rs:279-287`. A sync allows
100 lookups, producing up to 100 executor-blocking whole-cache writes and
O(A²)-shaped serialized bytes.

Update the reconstructible cache in memory, mark it dirty, and flush once at the
end of enrichment/sync via blocking work. Preserve accepted entries on partial
sync; a failed cache flush is non-fatal and remains dirty for retry.

Proof: 100 fake misses cause one persistence call, partial/error paths retain
entries, and a Tokio ticker remains responsive.

### [x] RUST-039 — Make Spotify catalog dirtiness reflect actual changes

**P2 · minor · R-P1, R-API1**

Catalog observation bumps generation even when merging identical data
(`retune-spotify/src/catalog.rs:73-234`, `:318-320`).
`flush_spotify_catalog` deep-clones before checking whether generation changed at
`lib.rs:913-925`, so a clean catalog is cloned every 30 seconds. Persisted
`local_hint` setters and `observed_uris` have no production reader.

Have merge/insert helpers report actual mutation and bump only then; compare the
cheap generation before cloning; remove unused fields/methods after a usage
search. Old unknown JSON fields are ignored, so no migration machinery is
needed.

Proof: identical observations keep generation stable, clean flushes do not
clone/write, one changed field bumps once, and old JSON with removed keys loads.

### [x] RUST-040 — Keep library/backup transformations typed

**P2 · minor · R-S1, R-P1**

`sync::without_fixtures` serializes `Library`, navigates a JSON pointer, filters,
then reimports at `sync.rs:248-262`. Backup export/import similarly parses the
core export into `serde_json::Value`, inserts/removes fields, reserializes, and
reparses at `lib.rs:2322-2428`.

For fixtures, clone and call `remove_uris`. For backup, define one typed v1 shell
envelope containing `version`, `Library`, and optional shell fields. Once
`RUST-002` makes `Library` deserialization self-validating, no Value surgery is
needed. Pin the existing byte-level compatibility shape with handwritten fixtures
before changing implementation.

Do not build a schema registry or generic serialization framework.

Proof: old core-only and full Retune v1 backups round-trip exactly in meaning;
unknown forward version remains rejected; fixture album-rating cleanup remains
correct.

### [x] RUST-041 — Replace source-text assertions with behavioral evidence

**P2 · minor · R-S1, R-T1**

Three Last.fm tests read `lastfm_import.rs`, split around function names, and
assert substrings/counts at `lastfm_import.rs:14994-15056`. Native CI likewise
reads `spotify_commands.rs` and searches `bind_on(8898)` plus a callback URL at
`.github/workflows/ci.yml:118-129`; the full URL currently appears in a comment,
so the check can pass without proving runtime behavior.

Expose one production callback port/path constant used by both flows and pin it
with Rust behavior tests. Replace Last.fm textual anchors with existing stats,
test-only counters, or large-input behavioral tests that measure bounded work.
Keep the CI JSON parse of `tauri.conf.json`; that is structured and valid.

Proof: renaming/reformatting functions does not break tests, while deliberately
reintroducing repeated projection work or changing the callback contract does.

### [x] RUST-042 — Await volume application directly

**P2 · minor · R-A1**

`playback_commands.rs:78-83` occupies a blocking-pool thread solely to call
`async_runtime::block_on` on an already-async `set_volume`. Rapid slider input can
consume blocking threads waiting on async locks/network work.

After the settings transaction is fixed, simply `.await` playback application.

Proof: existing fake-backend behavior and preference-first semantics remain
unchanged; no blocking task is spawned.

### [x] RUST-043 — Remove the unused Rodio output feature from `retune-audio`

**P2 · minor · R-D1**

`retune-audio/Cargo.toml:9` enables `rodio/playback`, which exists to pull CPAL,
while the crate uses only `Source`-side types. Desktop independently enables
playback and owns output devices.

Remove the feature from `retune-audio`; retain it in desktop. This makes
standalone audio tests/builds lighter and better matches the no-device crate
boundary.

Proof: `cargo tree -p retune-audio -e features` no longer contains CPAL, audio
tests pass, and the desktop graph/bundles still contain playback.

## P2: Cargo and maintainability

### [x] RUST-044 — Make the Rust-version policy truthful and enforce it

**P1 · major build-contract defect · R-D1**

Desktop declares Rust 1.85 at `apps/desktop/src-tauri/Cargo.toml:7`; other members
do not inherit a version and use edition 2024. The directly pinned
`symphonia-adapter-libopus 0.2.5` and transitive `ogg_pager 0.7.2` both declare
Rust 1.89. CI tests only moving stable. Cargo 1.85 therefore rejects the locked
graph before compilation.

Choose one policy and make every manifest/CI statement agree. The established
app/CI policy is “current stable,” so the smallest recommendation is to remove
the false 1.85 claim and explicitly retain stable-only support. If an MSRV is a
real product requirement, set workspace `rust-version = "1.89"`, inherit it in
all members, and add one locked 1.89 check.

Mixed editions are not a defect and should not be homogenized for appearance.

Proof: `cargo metadata` reports one intended policy; the selected minimum or
current stable runs `check --workspace --all-targets --locked` and the normal
suite.

### [x] RUST-045 — Resolve the `block 0.1.6` future incompatibility upstream

**P2 · minor now, future build failure · R-D1**

Clippy passes but reports that `block 0.1.6` will be rejected by a future Rust
release for an uninhabited static. The path is
`souvlaki 0.8.3 → cocoa 0.24.1 → block 0.1.6`; `cargo report
future-incompatibilities --id 1` confirms it.

Prefer a compatible `souvlaki`/upstream update or maintained replacement. Do not
fork/patch immediately unless upstream leaves no release path; this is visible
and not yet a current compile failure. Validate macOS media-key behavior after
the dependency change.

Proof: the future-incompatibility report is empty for the workspace and native
media-key tests/manual controls still pass.

### [x] RUST-046 — Split independent Last.fm/desktop responsibilities mechanically

**P2 · major locality debt · R-M1**

`lastfm_import.rs` is about 21,640 lines: roughly 11,300 production lines plus
about 10,000 lines of tests. It owns persisted models/stores, cache paths,
downloading, aggregation, matching, review projection, apply queue/worker,
reconciliation/journaling, and commands. `lib.rs` is about 4,566 lines and mixes
composition with settings, sync, view projection, library transactions,
playlists, backup/restore, and menus. Source-text tests are already compensating
for poor locality.

After correctness fixes, move one private seam at a time without behavior/API
change. A reasonable destination is
`lastfm_import/{model,store,source,matching,review,apply,reconcile,commands}.rs`
behind the existing `Service` facade, plus focused `backup.rs` and settings
ownership extracted from desktop `lib.rs`. Move tests with their owners.

Do not create a new crate, trait-per-module, dependency-injection framework, or
big-bang rewrite. File length is not the target; effect/invariant ownership is.

Proof after every mechanical move: unchanged visibility/public API, fmt,
Clippy, full tests, and no persistence fixture changes.

### [x] RUST-047 — Correct retired token-migration documentation

**P3 · minor · R-M1**

`docs/architecture/persistence.md:202-207` still describes legacy native token
migration that has been deliberately removed. State the current behavior: stale
installations reauthenticate. Do not recreate migration machinery merely to make
the old paragraph true.

## Historical non-findings: measure before changing

These were inspected and are plausible costs, but there is not enough evidence
to prescribe code yet:

- **Audio packet-buffer churn (R-P1):** `FileSource` allocates a fresh
  `SampleBuffer` and copies into `Vec<f32>` per packet. Profile allocations and
  underruns; reuse a buffer only if material.
- **Whole-library save per edit/play (R-P1):** JSON cloning and atomic replacement
  are intentionally simple and preserve commit semantics. Benchmark real 10k–50k
  libraries before proposing a journal or database.
- **Catalog growth/eviction (R-P1):** record serialized bytes, entity counts, and
  flush duration before inventing eviction.
- **Deferred normalized music buffering (R-P1):** profile peak memory on a real
  large library before restructuring genre enrichment.
- **Playback event channels (R-A1):** producers are low-rate and critical events
  complicate lossy bounds. Instrument queue depth before bounding/coalescing
  position events.
- **Connect one-second polling (R-A1):** measure request/rate-limit incidence and
  transient recovery before changing cadence; use adaptive backoff only if data
  supports it.
- **Last.fm backlog/receipt growth (R-P1):** measure actual persisted histories
  before choosing retention policy.
- **Directory fsync after rename (R-C1):** first decide whether “atomic” promises
  non-torn visibility or power-loss durability. Do not silently expand the
  product contract.
- **Non-UTF-8 string IPC path (R-C1):** native dialog/drag paths remain `PathBuf`.
  Confirm the string command is a real non-UTF-8 entry point before changing its
  frontend contract.
- **Settings `playback_backend`/`repeat` enums (R-API1):** convert validated
  strings to serde enums only when touching this boundary and only if persisted
  compatibility remains explicit. Do not wrap every preference primitive.

## Historical explicit non-actions

The following were reviewed and should remain as they are unless new evidence
appears:

- Keep first-party `unsafe` confined to the documented Windows
  `MoveFileExW` atomic-replacement call; do not manufacture broader R-U1 work.
- Keep `retune-core` free of filesystem, async, network, UI, and Tauri concerns.
- Keep the shared Spotify request gate. Its async lock intentionally spans wait
  and send to prevent a thundering herd.
- Keep the bounded loopback OAuth parser. It is a small deliberate textual
  grammar with loopback binding, state/path checks, 16 KiB header limit, and a
  global deadline; an HTTP-server dependency would be worse.
- Keep tolerant item-level Spotify page decoding. Fix callers that infer exact
  absence after `skipped > 0`.
- Keep Last.fm's four-page bounded download window and deterministic checkpoint
  order.
- Keep the existing Last.fm and release-token random `create_new` atomic writers;
  reuse their shape.
- Do not replace every poisoned-lock `expect`, locally proven serialization
  `expect`, test `unwrap`, or checked index with ceremony. The runtime panic audit
  found only the specific file-thread and decode/error issues above.
- Do not remove semantically clarifying clones merely because they exist. In
  particular, whole-library clones currently provide old-or-new in-memory commit
  semantics.
- Do not introduce a permanent dual library index until the transient batch
  indexes are measured and `tracks_mut` has an explicit consistency story.
- Do not add `IndexMap`, Loom, property-test frameworks, retry libraries, cache
  abstractions, schema registries, secret-zeroization crates, or cancellation
  frameworks for the listed repairs.
- Do not unify Rust editions for visual consistency.
- Cargo duplicate versions inspected were explainable ecosystem/platform splits;
  do not run a blanket dependency-deduplication campaign.
- The debug-only token feature is intentionally used by local build scripts;
  do not remove it. Keep release CI proving native encrypted storage instead.
- Diagnostics parsing/redaction is a bounded explicit log grammar and is not an
  R-S1 violation.
- Local filesystem paths generally stay as `Path`/`PathBuf`; display conversion
  is for messages. There is no broad UTF-8 rewrite to do.

## Historical rule coverage

| Rule | Result |
| --- | --- |
| R-C1 | Main work: path containment, validated state, atomic commits, bounds, audio/Spotify correctness |
| R-S1 | Typed errors/backup transforms, fixed Opus grammar, remove Rust source-text inspection |
| R-E1 | Fatal decode propagation, recoverable thread creation, useful typed/source errors |
| R-O1 | Intentional transaction clones retained; bulk/transient allocation issues targeted only where costly |
| R-U1 | One reviewed Windows-only atomic-replacement FFI boundary; no broader action |
| R-A1 | Aggregate gates, token epoch, RAII runners, blocking-IO placement, stale playback intent |
| R-P1 | Bulk indexes, one genre flush, projection/browse/catalog shape, hard input bounds |
| R-API1 | Validated cache/Spotify identities, account ID, protocol values, narrow error payloads |
| R-M1 | Staged Last.fm/desktop module split; correct stale documentation |
| R-D1 | Truthful toolchain, future incompatibility, remove unused audio output feature |
| R-T1 | Every item above names focused evidence; no ritual Miri/Loom/property-test demand |

## Completion criteria used

Each non-trivial item was considered complete only when:

1. its smallest owning-boundary regression proof fails on the old behavior and
   passes on the repair;
2. relevant persisted v1 fixtures and supported-platform behavior remain
   compatible or have an explicit migration;
3. the owning architecture document is updated when required;
4. `node scripts/check-docs.mjs`, `cargo fmt --all --check`,
   `cargo clippy --workspace --all-targets -- -D warnings`, and
   `cargo test --workspace` pass; and
5. Spotify changes are rechecked against current official documentation and
   manually exercised signed-in plus disconnected/error where credentials allow.

Native packaging, real audio output, and live account journeys remain separate
manual validation; CI must not acquire live credentials or require an audio
device.
