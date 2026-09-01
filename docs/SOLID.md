# SOLID audit completion record

Status: completed and independently re-audited, 2026-08-31. All findings
`SOLID-001` through `SOLID-040` are resolved in the authoritative tree.

This document is retained as the user-requested exhaustive audit record. The
item bodies below preserve the original pre-remediation evidence, proposed
changes, and proof criteria; they are not current architecture or an active
work queue. Current boundaries and invariants live in
[ARCHITECTURE.md](../ARCHITECTURE.md) and the owning documents under
[docs/architecture](architecture/). Current verification commands live in
[DEVELOPMENT.md](DEVELOPMENT.md).

Final independent verification covered 685 desktop Rust tests with one ignored
real-device test, 94 Spotify tests, 29 audio tests, 94 Node tests, 10 mounted
Vitest tests, and warning-free frontend lint. The complete workspace, strict
Clippy, documentation, frontend build, release-contract, Tauri ACL, and native
package gates also passed.

## Original executive verdict

Retune does not need a rewrite. Its foundation is better than the raw file sizes
suggest:

- retune-core is genuinely deterministic and inward-facing.
- Spotify Web API traffic uses one shared client and request gate.
- playback backends emit neutral events and the reducer owns queue advancement.
- local files remain usable without Spotify.
- the important persistence formats are versioned, individual file replacement
  is generally atomic, and release OAuth tokens are encrypted.
- the existing Rust test suite is broad, especially around matching, playback,
  persistence recovery, Spotify request policy, and Last.fm apply journals.

The material debt is concentrated at ownership boundaries:

1. Settings, playlists, tokens, sync, and backup restore have competing or
   incomplete commit owners. These are correctness and durability issues, not
   style complaints.
2. Last.fm import behavior has accumulated in two mutually coupled services,
   one 21,640-line importer module, thick Tauri commands, and a 1,170-line
   frontend coordinator.
3. Typed lower-level failures are often flattened to prose at the application
   boundary and later parsed back into policy.
4. Several frontend async flows do not bind results to the request, account, or
   entity that produced them.
5. Frontend tests and lint exist locally but are not enforced in CI, and many
   green UI tests inspect source spelling rather than behavior.

The correct strategy is a sequence of small, behavior-protected boundary
repairs followed by mechanical module splits. The wrong strategy would be a
big-bang rewrite, a DI container, an interface per file, or moving
account/network-aware matching into retune-core.

## Scope and method

The requested standard was the language-agnostic reviewer at:

    /Users/rianjs/monit/monit-claude-plugins/.codereview/agents/architecture/solid-reviewer-agnostic/prompt.md

That prompt normally reviews a diff. This audit deliberately expands its scope
to the entire then-current repository. Findings use its rule IDs:

| Rule | Meaning used here |
| --- | --- |
| U-S1 | One nameable responsibility per module or owner |
| U-S2 | Commands and entry points translate, call, and render |
| U-S3 | Computation remains separable from effects |
| U-O1 | Extension points are earned; closed domains use closed types |
| U-L1 | Implementations and async results honor their behavioral contracts |
| U-L2 | Error kinds survive until policy decisions are finished |
| U-I1 | Contracts expose only what consumers need |
| U-D1 | Collaborators and effects are explicit |
| U-D2 | Construction happens at one composition root |
| U-T1 | Non-trivial behavior has executable regression proof |
| U-T2 | CI and static-analysis ratchets only tighten |
| U-G1 | Behavior fixes and mechanical refactors stay reviewable |

The audit covered:

| Area | Primary files inspected |
| --- | --- |
| Core model and projection | crates/retune-core/src/model.rs, browse.rs, io.rs |
| Spotify transport and storage | crates/retune-spotify/src/auth.rs, client.rs, catalog.rs, tokens.rs |
| Local audio | crates/retune-audio/src/lib.rs, import.rs and desktop file playback |
| Desktop composition | apps/desktop/src-tauri/src/lib.rs, store.rs, sync.rs, provider.rs |
| Spotify and playlists | spotify_commands.rs, playlist_commands.rs, playlists.rs |
| Playback | playback/mod.rs, reducer.rs, connect.rs, local.rs, file.rs, playback_commands.rs |
| Last.fm | lastfm.rs, lastfm_import.rs and the matching/persistence architecture docs |
| Frontend | App.tsx, LastFmImporter.tsx, spotifyViews.tsx, dialogViews.tsx, reducers and helpers |
| Enforcement | apps/desktop/test/ui.test.ts, package scripts, CI, docs checks, project manifests |

The inspected Rust and TypeScript sources and tests total roughly 58,000 lines.
The largest concentration points are:

| File | Lines | Relevant observation |
| --- | ---: | --- |
| apps/desktop/src-tauri/src/lastfm_import.rs | 21,640 | Production through about line 11,365; tests after it |
| apps/desktop/src-tauri/src/lib.rs | 4,566 | Composition plus settings, sync, backup, menus, and library effects |
| apps/desktop/src-tauri/src/lastfm.rs | 2,921 | Auth, HTTP, storage, queueing, listening, and UI events |
| crates/retune-spotify/src/client.rs | 2,585 | Transport, request policy, endpoint facade, models, and tests |
| apps/desktop/src-tauri/src/provider.rs | 2,551 | Sync lifecycle plus view/search mapping |
| apps/desktop/src/App.tsx | 1,389 | Root composition plus several feature controllers |
| apps/desktop/src/LastFmImporter.tsx | 1,170 | Import refresh, mutation, draft, focus, and presentation ownership |
| apps/desktop/test/ui.test.ts | 1,069 | Pure tests mixed with source-text assertions |

Line count alone is not a finding. Each proposed split below is justified by a
specific ownership, contract, or testability failure.

## Original verification baseline

The worktree was clean before this audit. The following checks were run against
the audited revision:

| Check | Result |
| --- | --- |
| node scripts/check-docs.mjs | Passed after this document and its AGENTS route were added |
| cargo fmt --all --check | Passed |
| cargo clippy --workspace --all-targets -- -D warnings | Passed |
| cargo test --workspace | 601 passed, 1 ignored |
| npm run test | 69 passed |
| npm run lint | Exited successfully with 7 exhaustive-deps warnings |
| npx tsc --noEmit -p tsconfig.app.json | Passed |
| strict TypeScript probe for the app config | Passed without production changes |
| npm run build | Passed |

At the original snapshot, the seven lint warnings were at:

- apps/desktop/src/dialogViews.tsx:43 and :86
- apps/desktop/src/spotifyViews.tsx:175 and :308
- apps/desktop/src/App.tsx:290, :367, and :1335

Passing tests were useful evidence, but they did not disprove the concurrency,
failure-injection, stale-result, or rendered-interaction defects below because
those scenarios were not executed by the original baseline.

## Historical priority and size key

- P0: correctness, account isolation, or durable-state integrity; repair before
  structural cleanup in the affected flow.
- P1: important contract and regression-test work that prevents recurring
  defects.
- P2: structural work with a concrete, already-earned seam.
- P3: safe opportunistic cleanup or mechanical work after behavior is stable.
- Size S/M/L/XL is relative implementation and review size, not a calendar
  promise. XL items must be delivered as a sequence of smaller changes.

## Original strengths to preserve

These were explicit non-findings preserved during remediation.

1. retune-core has no filesystem, network, async, UI, or Tauri dependencies.
   Its io module is bytes-in/bytes-out, so the project purity invariant is met.
2. main.rs and main.tsx are thin. apps/desktop/src-tauri/src/lib.rs:2555 is a
   real composition root even though too many unrelated operations still live
   in the same file.
3. Spotify Transport, TokenStore, OverlayStore, SessionStore, and MediaProvider
   have real implementations and real fakes or alternatives. They are earned
   seams, not speculative abstractions.
4. The Spotify Web API wait-and-send gate at
   crates/retune-spotify/src/client.rs:913-949 is a correct shared boundary and
   has a concurrent-caller regression test.
5. The playback reducer at
   apps/desktop/src-tauri/src/playback/reducer.rs:160 onward is a strong pure
   core. The closed PlayerBackend enum is idiomatic Rust and should not become a
   trait hierarchy.
6. The library clone, save, then swap transaction at
   apps/desktop/src-tauri/src/lib.rs:1932-1966 is the pattern other durable
   mutations should reuse.
7. Last.fm importer serialized mutations and its durable apply/application
   journals are justified complexity. The problem is responsibility
   concentration around them, not the existence of recovery logic.
8. The frontend appState reducer and Spotify pagination generation logic are
   useful pure cores. Do not fragment appState merely because it is central.
9. Best-effort event emission is acceptable where events are only invalidation
   hints and a later state fetch recovers. Do not turn every ignored event
   result into a bespoke error type.
10. Audio tag-read failure intentionally falls back to decodable stream
    metadata. That is a reasonable degradation and is distinct from losing a
    directory scan or misreporting a decode failure as EOF.

## P0 — correctness and durable-state ownership

### [x] SOLID-001 — Use one concurrency-safe atomic file primitive

Rules: U-S1, U-D2, U-L1, U-T1  
Size: M  
Dependencies: none; land before SOLID-002 and SOLID-003.

Evidence:

- apps/desktop/src-tauri/src/store.rs:859-875 always writes through one fixed
  extension such as settings.json.tmp using create and truncate.
- apps/desktop/src-tauri/src/lastfm.rs:298-335 independently implements a
  stronger random, create-new, permission-aware temporary file.
- apps/desktop/src-tauri/src/lastfm_import.rs:3029, :3075, :3169, :3261, and
  :3268 reach sideways into lastfm only to reuse that filesystem behavior.
- crates/retune-spotify/src/tokens.rs:273-292 has another unique-temp atomic
  implementation.

Why it matters:

Two writers to the same persisted file can truncate or rename the same
temporary path. One command can fail after another command has already renamed
the shared temp file, leaving disk and memory on different revisions. Multiple
private implementations also make durability and permission changes land
unevenly.

Smallest acceptable change:

- Move the strongest existing unique-temp implementation to one persistence
  utility in the desktop persistence layer.
- Use a unique same-directory name, create_new, file sync, rename, cleanup on
  failure, and an optional Unix mode.
- Reuse it from general stores and Last.fm. Keep the crypto-specific token
  wrapper, but let it call the same primitive where crate boundaries permit;
  do not create a filesystem service hierarchy.

Done when:

- Barrier-controlled concurrent writes never share a temporary path.
- The final file is always one complete input, never an interleaving.
- Failure removes only the failing writer's temporary file.
- Existing Unix permission and atomic-replacement tests remain green.

### [x] SOLID-002 — Give settings one mutation and persistence owner

Rules: U-S1, U-D1, U-I1, U-L1, U-T1  
Size: L  
Dependencies: SOLID-001.

Evidence:

- apps/desktop/src-tauri/src/lib.rs:755-801 clones the full Settings value,
  releases the mutex, awaits Last.fm state, then saves and replaces the whole
  snapshot.
- lib.rs:804-827, :845-865, :966-982, :1409-1420, and :2517-2538 repeat
  independent clone, modify, save, and swap flows.
- apps/desktop/src-tauri/src/playback_commands.rs:60-84, :87-103, :106-124,
  and :127-176 do the same for volume, repeat, shuffle, and audio settings.
- apps/desktop/src/App.tsx:279-292 autosaves the full frontend snapshot.
  App.tsx:408-413 and :705-721 also issue direct settings/audio writes after
  dispatching changes that can trigger autosave.
- get_settings and set_settings expose the storage model at lib.rs:745-802.
  set_settings must restore backend-owned spotify_sync_completed and
  last_full_sync at :764-765.
- Rust persists last_full_sync at store.rs:118-120, while the TypeScript
  Settings shape at apps/desktop/src/types.ts:11-40 omits it.
- apps/desktop/src/LastFmImporter.tsx:1051-1059 fetches the entire settings
  record only to read theme.

Why it matters:

Concurrent commands can start from the same old snapshot and silently revert
each other's unrelated fields. A full-snapshot frontend save can overwrite a
sync timestamp written while set_settings was awaiting Last.fm. Duplicate UI
writes make the race routine rather than theoretical. The public IPC contract
is also coupled to private persisted bookkeeping.

Smallest acceptable change:

- Introduce one native settings owner, either a small SettingsState or one
  mutate_settings helper guarded across latest-value clone, patch,
  normalize/validate, save, swap, and event emission.
- Route every native writer through it.
- Replace full-snapshot IPC writes with narrow field patches or intent-specific
  commands. Keep the on-disk Settings format unchanged.
- Expose a user-facing settings view that excludes sync bookkeeping. Give the
  importer a theme/appearance payload rather than Settings.
- Remove the JSON-derived autosave and direct-plus-effect duplicate writes.

Done when:

- A deterministic two-writer test changes different fields and both survive in
  memory and on disk.
- A sync timestamp written while another settings operation is delayed cannot
  be reverted.
- Frontend code cannot send spotify_sync_completed or last_full_sync.
- One user action produces one durable settings mutation and one change event.

### [x] SOLID-003 — Serialize playlist cache mutations at their owner

Rules: U-D1, U-L1, U-T1  
Size: M  
Dependencies: SOLID-001.

Evidence:

- apps/desktop/src-tauri/src/playlist_commands.rs:39-48, :51-65, :201-224,
  :250-273, and :294-307 clone the entire cache, sometimes await Spotify, then
  replace the entire cache.
- apps/desktop/src-tauri/src/lib.rs:1832-1858 repeats that flow for add.
- lib.rs:1439-1461 syncs from a cloned cache without a playlist mutation gate.
- lib.rs:1860-1869 saves and then overwrites all in-memory playlist state.
- AppState has a playlist mutex and store at lib.rs:96-97 but no mutation gate
  spanning remote work and commit.

Why it matters:

Two successful Spotify operations can both start from the same local cache;
the later local commit drops the earlier result. Reorder, follow/unfollow, add,
create, and sync can overwrite one another. A later sync may repair some cases,
but current UI state and persisted state are wrong in the meantime.

Smallest acceptable change:

- Add one async playlist mutation gate around clone, remote operation, save,
  swap, and event emission for every mutation and sync.
- Keep the existing pure playlists functions.
- Start with one global playlist gate. Per-playlist locks are unnecessary until
  measured contention proves otherwise.

Done when:

- A controlled create/create or create/reorder interleaving preserves both
  successful operations.
- Sync cannot overwrite a mutation that completed after the sync snapshot was
  taken.
- A failed save leaves the previous in-memory cache intact.

### [x] SOLID-004 — Make token lifecycle transitions linearizable

Rules: U-L1, U-L2, U-I1, U-T1  
Size: M  
Dependencies: none.

Evidence:

- TokenStore exposes independent load, save, and clear operations at
  crates/retune-spotify/src/tokens.rs:75-79.
- CachedTokenStore writes its backing store before taking the cache lock at
  tokens.rs:123-143.
- refresh_token checks the stored access token, awaits HTTP, then
  unconditionally saves at crates/retune-spotify/src/client.rs:952-987.
- replacement OAuth grants save at
  apps/desktop/src-tauri/src/spotify_commands.rs:107-142.
- explicit disconnect clears at spotify_commands.rs:240-261.

Why it matters:

Interleaved save and clear calls can leave the in-process cache and backing file
on different revisions. A delayed refresh for account A can also overwrite an
explicit disconnect or a replacement grant for account B, resurrecting old
credentials across an account boundary. The per-client refresh lock cannot
coordinate an old client, a newly constructed client, and disconnect.

Smallest acceptable change:

- Hold one token lifecycle lock across each backing-store plus cache transition.
- Add a conditional replacement operation such as replace_if_current using the
  expected access token or grant revision.
- Refresh must discard its response when the stored grant changed while HTTP
  was in flight.
- Keep the fix at TokenStore/CachedTokenStore; adding more shell locks would
  leave old and replacement clients uncovered.

Done when:

- Barrier-controlled save/clear tests always leave cache and backing store
  equal.
- Delayed refresh plus clear leaves the store empty.
- Delayed refresh plus replacement grant preserves the replacement grant and
  its playback-credential policy.

### [x] SOLID-005 — Reconcile Spotify sync on a working copy, then commit once

Rules: U-S3, U-L1, U-T1  
Size: L  
Dependencies: SOLID-001; coordinate with SOLID-003 for playlist sync.

Evidence:

- apps/desktop/src-tauri/src/lib.rs:1280-1293 applies callback batches directly
  to the live AppState library before the snapshot succeeds or is durable.
- lib.rs:1369-1378 later applies the final result to that already-mutated live
  value.
- apps/desktop/src-tauri/src/sync.rs:99-114 mutates its Library argument and
  only then calls the store.
- lib.rs:1932-1966 already demonstrates the safer clone, mutate, save, then
  swap transaction.

Why it matters:

A provider failure after an early batch leaves UI-visible additions that were
never saved. A final filesystem error can leave additions or destructive
pruning in memory while library.json remains old. The command reports failure,
but a later unrelated edit may accidentally persist the partial sync.

Smallest acceptable change:

- Have reconciliation produce a candidate Library or a mutation delta without
  owning persistence.
- Keep progressive batches in a sync-owned working snapshot if progress must
  remain visible.
- Commit the accepted final candidate through the existing library transaction
  boundary exactly once.
- Decide and document whether a partial-but-valid Spotify snapshot is a
  committable result; a terminal error must not leak working state.

Done when:

- A provider that fails after its first batch leaves live and persisted library
  state unchanged.
- A failing OverlayStore leaves live state unchanged.
- A successful sync still reports progress and produces the same final library.

### [x] SOLID-006 — Make backup replacement recoverable across all component files

Rules: U-S1, U-L1, U-T1  
Size: L  
Dependencies: SOLID-001, SOLID-002, and SOLID-003.

Evidence:

- apps/desktop/src-tauri/src/lib.rs:2380-2428 validates/deserializes the backup
  components before applying them.
- lib.rs:2472-2515 then commits library, settings, playlists, and Last.fm
  mappings in order.
- Failures at :2491, :2498, or :2505 return after earlier components are
  already durable.

Why it matters:

Replace can report failure after only a subset of the library, preferences,
playlists, and mappings has been replaced. Atomic replacement of each file does
not make the bundle transactional, and the operation is explicitly presented
as a whole-library replacement.

Smallest acceptable change:

- Add a small durable restore journal containing enough before/after state to
  finish or roll back deterministically.
- Validate all components first, write the journal, apply each component
  through its normal owner, then clear the journal.
- Recover an incomplete restore during startup.
- If the journal must be deferred, return a structured partial-restore result
  and stop presenting the operation as all-or-nothing; that is an interim
  contract, not the final durability fix.

Done when:

- Failure injection at every component boundary leads to deterministic startup
  recovery.
- Successful restore updates all owners and emits one coherent refresh.
- Merge, which does not replace settings/playlists/mappings, retains its current
  narrower behavior.

### [x] SOLID-007 — Do not convert failed importer mutations into success

Rules: U-L1, U-L2, U-T1  
Size: S  
Dependencies: none; fix before moving importer files.

Evidence:

- apps/desktop/src/LastFmImporter.tsx:442-449 catches a command failure,
  reports it, and returns an empty array.
- toggleAlbumSkip at :522-527 still announces success and may advance after
  that empty result.
- Ignore Album and Ignore Artist at :767 call then(onNext), so the caught
  failure still navigates.
- The adjacent runPageMutation path at :450-457 already returns null and its
  callers check for success.

Why it matters:

The UI tells the user a durable review decision succeeded and moves away from
the batch even though the backend rejected it. This hides pending work and can
cause the user to trust a state that was never saved.

Smallest acceptable change:

- Let the helper reject or return an explicit success/failure discriminant.
- Announce success and advance only after success.
- Reuse the adjacent checked-result pattern; do not add a generic mutation
  framework.

Done when:

- Rejected skip, ignore-album, and ignore-artist calls leave the batch selected,
  show the error, and show no success status.
- Successful calls retain current navigation behavior.

### [x] SOLID-008 — Key playlist rows to the playlist that produced them

Rules: U-S1, U-L1, U-T1  
Size: S to M  
Dependencies: none; independent of the native mutation gate.

Evidence:

- PlaylistView owns an unkeyed tracks array at
  apps/desktop/src/App.tsx:1042.
- Its identity-change effect at :1062-1074 resets selection but does not clear
  old rows before loading the new playlist; an error also leaves them visible.
- canChangePlaylist trusts ownership and row count at :1059-1060.
- The parent reuses the same component instance at App.tsx:597-630.
- Old rows remain playable and mutation actions use the new playlist props at
  :1090 and :1254-1303.

Why it matters:

Selecting playlist B initially displays playlist A. If both have the same track
count, reorder or remove can send B's ID with indices derived from A. Even when
counts differ, stale rows remain playable and actionable.

Smallest acceptable change:

- Key PlaylistView by playlist ID.
- Store remote data as playlist ID plus status plus rows, and render/action it
  only when the stored ID equals the current prop.
- Clear or invalidate rows synchronously on identity change.

Done when:

- A to B switching with equal counts, delayed B, failed B, and late A response
  never renders or mutates A rows under B.

### [x] SOLID-009 — Preserve late audio decode failure as failure, not natural EOF

Rules: U-L1, U-L2, U-I1, U-T1  
Size: M  
Dependencies: none.

Evidence:

- crates/retune-audio/src/lib.rs:147-183 logs non-EOF packet errors and decoder
  errors, marks the source finished, and returns false.
- Iterator::next at :188-198 converts that result to None, indistinguishable
  from a clean end of stream.
- apps/desktop/src-tauri/src/playback/file.rs:307-316 emits EndOfTrack whenever
  the sink becomes empty.
- The backend already emits Unavailable for load failures at
  playback/file.rs:238-245.

Why it matters:

A truncated or damaged file can open and play initial packets, then be treated
as naturally completed. The reducer may advance the queue and record completion
instead of reporting an unavailable source.

Smallest acceptable change:

- Give FileSource a small shared completion/error status that the file worker
  can inspect when the sink empties.
- Reuse NeutralEvent::Unavailable for a late decode failure.
- Do not redesign rodio Source or predecode whole files.

Done when:

- A fixture that yields PCM and then fails records the decode error.
- The backend emits Unavailable and never EndOfTrack for that fixture.
- Clean EOF behavior is unchanged.

## P1 — contracts, async identity, and enforcement

### [x] SOLID-010 — Preserve structured errors until policy decisions are complete

Rules: U-L1, U-L2, U-I1  
Size: L, delivered one domain at a time  
Dependencies: start with importer apply; supports SOLID-030.

Evidence:

- crates/retune-spotify/src/lib.rs:13-52 already defines typed transport,
  rate-limit, quota, server, JSON, token, and request errors.
- apps/desktop/src-tauri/src/spotify_commands.rs:6-38 reduces Spotify action
  failures to String after recording some cooldown information.
- apps/desktop/src-tauri/src/lastfm_import.rs:5464-5475 reconstructs policy by
  parsing the prefixes Spotify rate limited and Spotify Development Mode quota
  exhausted.
- The apply worker uses that parser at lastfm_import.rs:9517-9525.
- apps/desktop/src/LastFmImporter.tsx:32-47 parses the same English prefixes
  again to classify the UI state.
- apps/desktop/src-tauri/src/playlists.rs:422-430 maps a scope/reconnect
  condition to display text, and lib.rs:1469-1482 compares that text to
  RECONNECT_HINT to choose behavior.
- CollectionAlbumDialog accepts arbitrary command names and a broad union
  result at LastFmImporter.tsx:282-292, so command and error semantics are both
  discovered at runtime.

Why it matters:

Changing punctuation, wording, or localization can silently change retry
scheduling, reconnect behavior, and rate-limit UI. Result-shape drift also
compiles in TypeScript and fails only after invocation.

Smallest acceptable change:

- Carry a small domain/application error through each policy boundary. For
  Spotify apply, it needs at least kind, user message, and optional retryAt.
- Carry a typed playlist reconnect variant until the Tauri adapter.
- Serialize a compact Tauri error payload such as code, message, and retryAt.
- Render prose once at the UI edge.
- Do not replace every Result value with a global error framework. Display-only
  failures can remain simple messages.

Done when:

- Changing Error display text cannot change cooldown persistence, worker
  retryAt, reconnect suppression, or frontend limit classification.
- Representative success, retryable failure, quota failure, permanent failure,
  and malformed boundary payloads have contract tests.

### [x] SOLID-011 — Use one generation for importer state and queue refresh

Rules: U-S1, U-L1, U-T1  
Size: S to M  
Dependencies: none; fix before frontend importer extraction.

Evidence:

- Full refresh and queue-only refresh own independent counters at
  apps/desktop/src/LastFmImporter.tsx:917-918.
- Both write state, showQueries, queue, and defaults at :944-973 and :985-1001.
- Events and completion handlers can launch both modes at :1022-1034.
- Each rejects stale responses only relative to its own counter.

Why it matters:

A newer queue-only response can land first and then be overwritten by an older
full response whose separate counter still considers it current.

Smallest acceptable change:

- Use one authoritative request generation/coordinator for fields shared by
  both refresh modes.
- Page loading can retain its own identity because it owns disjoint state.
- Splitting the state into truly disjoint owners is valid but larger; prefer one
  generation first.

Done when:

- A cross-mode out-of-order test proves an older full refresh cannot overwrite
  a newer queue-only result and vice versa.

### [x] SOLID-012 — Correlate playback authorization with the latest play intent

Rules: U-S1, U-L1, U-T1  
Size: M  
Dependencies: coordinate with SOLID-030 if the IPC payload needs a request ID.

Evidence:

- usePlayer records the latest target in starting.current at
  apps/desktop/src/App.tsx:65-79.
- A later play_tracks outcome unconditionally stores and dispatches its captured
  authorization prompt at :79-86; its identity check only controls clearing
  starting.current.
- The event path at :50-54 dispatches a prompt even when
  pendingPlaybackTarget finds no matching current target.

Why it matters:

A slow authorization response for play A can override a newer play B. After
authorization, Retune may retry the stale queue and origin.

Smallest acceptable change:

- Give every play start a generation/request ID and ignore outcomes or events
  that are no longer current.
- Add the request identity to the native prompt payload only if URI/track
  identity is insufficient.
- Follow the existing native playback reducer generation pattern rather than
  inventing a global request manager.

Done when:

- In an A then B test where A resolves last, only B can open or populate the
  authorization prompt and retry state.

### [x] SOLID-013 — Bind every remote frontend result to its request identity

Rules: U-S1, U-L1, U-T1  
Size: M to L, delivered per view  
Dependencies: SOLID-030 makes fake-invoker tests easier.

Evidence:

- SpotifySearch copies query/navigation props into local state in an effect at
  apps/desktop/src/spotifyViews.tsx:392-413, so a new query can first render old
  local results while actions use the new query at :414-435 and :495-515.
- SpotifyAlbumPage clears old data only after render and does not validate the
  page URI against the current entry at spotifyViews.tsx:161-181.
- SpotifyArtistPage loadMore can resolve after navigation and write the prior
  artist's page at :320-335.
- Album and artist page instances are not keyed by identity at :475-482.
- App.openInfo has no generation/cancellation guard for rapid A to B or close
  transitions at apps/desktop/src/App.tsx:215-225.
- The good precedent is spotifySearch.ts:113-133, which uses generations and
  deduplicates pages.

Why it matters:

Old content can be displayed and acted on under a new query, album, artist, or
track identity. Effect cleanup alone does not protect handler-started requests.

Smallest acceptable change:

- Represent remote state as key, status, and data; render or mutate only when
  the key equals current props.
- Key album and artist view instances by URI/ID.
- Apply a small local generation to handler-started requests such as loadMore
  and openInfo.
- Reuse the existing Spotify pagination generation helper where its shape fits.

Done when:

- Delayed A to B navigation, query replacement, close-before-result, and late
  pagination tests never expose A under B or reopen a closed view.

### [x] SOLID-014 — Remove process-global frontend caches with unclear identity

Rules: U-D1, U-L1, U-L2, U-S1  
Size: S to M  
Dependencies: none.

Evidence:

- apps/desktop/src/spotifyViews.tsx:255-280 defines four mutable module-global
  maps for artist pages, artist requests, albums, and album requests.
- ArtistPageView includes account-specific following state, and album records
  include membership such as inLibrary, but keys contain only artist ID/offset.
- Unmount on sign-out does not clear module state.
- apps/desktop/src/App.tsx:749-775 defines another process-global artwork map.
  It caches transient invocation failure as null, indistinguishable from a
  successful no-artwork result.
- Native artwork and Spotify catalog caching already exist.

Why it matters:

A second Spotify account can see the first account's follow or membership
assumptions. Maps grow for the process lifetime. A transient artwork failure
suppresses retry until restart. Multiple cache owners obscure invalidation.

Smallest acceptable change:

- Delete the frontend authoritative Spotify and artwork caches and rely on the
  existing native owners.
- If measurement later proves a frontend cache necessary, key account facts by
  stable account ID, clear them on identity change, bound metadata entries, and
  cache successes rather than transport failures.

Done when:

- Sign out of account A, connect B, and revisit the same artist performs a
  fresh account-state read.
- A transient artwork failure can recover without restarting.

### [x] SOLID-015 — Return scan failures without discarding partial local-file work

Rules: U-L1, U-L2, U-I1, U-T1  
Size: S  
Dependencies: none.

Evidence:

- crates/retune-audio/src/import.rs:21-33 returns only Vec<PathBuf>, converts
  any recursive scan error into an empty list, and drops canonicalization
  failures.
- A broken symlink or unreadable child aborts recursion at import.rs:54-72.
- apps/desktop/src-tauri/src/localfiles.rs:39-68 validates only the supplied
  root, then has no error information to add to ImportSummary.failed.

Why it matters:

One bad child can make all valid audio under the selected directory disappear
while the UI reports neither imports nor failures. Users cannot distinguish an
empty directory from an aborted scan.

Smallest acceptable change:

- Return discovered paths plus path-specific failures. A Result of all paths is
  the minimum, but partial successes and failures better match ImportSummary
  without much additional code.
- Continue skipping hidden paths and not following directory symlinks.

Done when:

- A directory containing one valid audio file and one broken symlink imports
  the valid file and reports the broken path.
- A deterministic canonicalization failure is reported where the platform
  allows one.

### [x] SOLID-016 — Define one TokenStore corruption contract

Rules: U-L1, U-L2  
Size: S to M  
Dependencies: coordinate with SOLID-004.

Evidence:

- EncryptedFsTokenStore returns Ok(None) for short ciphertext, authentication
  failure/wrong key, and invalid plaintext JSON at
  crates/retune-spotify/src/tokens.rs:220-247.
- Its tests pin that behavior at tokens.rs:591-609.
- The debug FsTokenStore returns an error for invalid JSON at
  apps/desktop/src-tauri/src/store.rs:828-836.
- TokenStore at tokens.rs:75-79 does not document which semantic applies.

Why it matters:

None normally means credentials do not exist. The same damaged state silently
logs a user out in release but errors in development. Treating corruption as
absence also makes a later grant overwrite the best recovery/diagnostic
evidence.

Smallest acceptable change:

- Define a typed corrupt/undecryptable store error and align both
  implementations.
- Let the desktop shell quarantine the file and present a reconnect/recovery
  notice.
- Preserve Ok(None) only for a genuinely missing record.

Done when:

- One conformance matrix covers missing, valid, malformed, wrong key where
  applicable, clear idempotence, and ordinary I/O failure for both stores.

### [x] SOLID-017 — Enforce the frontend checks that already exist

Rules: U-T1, U-T2  
Size: S  
Dependencies: fix the seven warnings before denying warnings.

Evidence:

- docs/DEVELOPMENT.md:115-127 lists npm run test and npm run lint as the same
  checks used by CI.
- .github/workflows/ci.yml:29-45 runs npm ci, TypeScript, and build, but neither
  frontend test nor lint.
- apps/desktop/package.json:6-11 already defines both scripts.
- Current lint has seven exhaustive-deps warnings listed in the verification
  section. App.tsx:367 can retain a simulated tick interval across a backend
  mode change because simulated is missing.
- A strict TypeScript app probe passes with no production edits, while
  tsconfig.app.json:2-23 does not enable strict.

Smallest acceptable change:

- Fix all seven warnings by correcting ownership or using stable callbacks/
  callback refs where refetch on callback identity is not intended.
- Run oxlint with denied warnings and add lint plus test to frontend CI.
- Enable strict in the app and node TypeScript configs after verifying both.
- Do not add a second redundant typecheck if the existing build already covers
  the same project in a later step.

Done when:

- CI fails for a broken frontend test or hook warning.
- Local documented checks and CI commands agree.
- The simulated timer cleans up when simulated mode changes.

### [x] SOLID-018 — Replace test-shaped code with executable behavior proof

Rules: U-T1, U-T2, U-G1  
Size: L, incremental  
Dependencies: pair each replacement with the behavior it protects.

Evidence:

- apps/desktop/test/ui.test.ts imports readFileSync at :2 and contains extensive
  source/CSS spelling assertions at :138-146, :315-332, and :492-642.
- The only frontend test command mounts no React component, focuses no control,
  runs no effect, and crosses no fake Tauri boundary.
- Several exported decision helpers in
  apps/desktop/src/lastfmImportState.ts:377-462 and :511-515 exist only for
  tests; Rust is the real mutation/decision owner.
- apps/desktop/src-tauri/src/lastfm_import.rs:2418-2437 allows a helper to be
  dead in production; its only caller is its test at :17891. Production uses a
  different classifier at :6834-6871.
- The log-secret test at lastfm.rs:2901-2919 scans only lines containing log,
  so multiline arguments can evade it.
- Native CI at .github/workflows/ci.yml:118-129 searches Spotify command source
  for bind_on(8898) and a callback string instead of executing redirect
  behavior.

Why it matters:

Green tests can prove a parallel model or exact source spelling while real
mutation failures, stale results, focus, and redirect behavior remain broken.
Harmless refactors fail regex tests; behavior regressions can preserve the
matched text.

Smallest acceptable change:

- Keep node:test for genuine pure reducers, sorting, pagination, geometry, and
  command coordinators.
- Delete test-only shadow decision helpers and direct important cases to the
  native owner or a real boundary coordinator.
- Replace source assertions as each affected behavior changes; do not migrate
  all tests in one PR.
- Add the smallest mounted React harness only for effects, focus, and native
  control behavior that cannot be proved as a pure transition.
- Move OAuth port/path to one constant and test listener/redirect behavior in
  Rust.

Done when:

- Renaming markup without changing behavior stays green.
- Breaking each protected interaction or command mapping fails an executable
  test.
- No production dead-code allowance exists solely to support a test.

### [x] SOLID-019 — Restore native keyboard and control contracts

Rules: U-L1, U-I1, U-T1  
Size: M to L  
Dependencies: mounted interaction proof from SOLID-018.

Evidence:

- The window key handler ignores only input, textarea, and select at
  apps/desktop/src/App.tsx:432-445.
- It prevents Space for playback at :466-468, printable keys for typeahead at
  :469-487, and arrow keys at :488-510.
- Track rows are clickable/draggable div elements without focus or keyboard
  semantics at apps/desktop/src/libraryViews.tsx:212-227 and
  App.tsx:1254-1303.
- Spotify rows use the same pattern at spotifyViews.tsx:30-39, :61-66, and
  :511-515.
- Seek is implemented as a clickable progress output at App.tsx:848-857.
- ContextMenu is a bare div with document listeners but no focus entry,
  menu/item semantics, or focus restoration at
  apps/desktop/src/viewShared.tsx:45-71.

Why it matters:

Space on a focused button can toggle playback instead of activating the button.
Links, editable content, selection, open/play, seek, and context actions do not
consistently honor native keyboard behavior.

Smallest acceptable change:

- Exclude buttons, links, contenteditable elements, and interactive roles from
  unmodified global shortcuts. Keep explicit command-modifier shortcuts.
- Use buttons/links for actions and a range input for seeking.
- Where multi-select rows must remain custom, implement one established
  list/grid keyboard contract at the owning list component.
- Give retained context menus focus entry, Escape restoration, and menu/item
  semantics.

Done when:

- Focused button plus Space, link plus Enter, editable text, context-menu
  Escape/focus restoration, keyboard row actions, and range seeking have
  rendered tests.

### [x] SOLID-020 — Remove the hidden final-section precondition from MediaProvider

Rules: U-L1, U-I1, U-T1  
Size: M  
Dependencies: coordinate with SOLID-005.

Evidence:

- MediaProvider presents independent library_snapshot(kind, callback) calls at
  apps/desktop/src-tauri/src/provider.rs:152-164.
- LibraryKind::ALL requires Audiobooks to remain last because music is buffered
  at provider.rs:33-45.
- pending music is flushed only when kind equals Audiobooks at
  provider.rs:749-788 and :962-969.
- sync::snapshot happens to call every kind in order at
  apps/desktop/src-tauri/src/sync.rs:37-71.
- The direct MediaProvider implementation for SpotifyClient at
  provider.rs:990-998 has no equivalent sequencing precondition.

Why it matters:

Two implementations of one contract have different hidden call-order
requirements. Calling Tracks alone, selecting a subset, or adding/reordering a
future section can silently return no music.

Smallest acceptable change:

- Make a complete sync run one explicit operation such as snapshot_all, keeping
  per-section progress callbacks internal.
- An explicit finish/finalize call is acceptable but easier to misuse; prefer a
  single complete-run operation if it remains small.
- Sequential fetching and genre enrichment remain valid.

Done when:

- Calling the public run contract cannot omit buffered music.
- Reordering or adding a section cannot control finalization.
- Existing complete-run output remains equivalent.

### [x] SOLID-021 — Reject local URIs at the shared Spotify write boundary

Rules: U-S1, U-I1, U-T1; project Spotify write invariant  
Size: S  
Dependencies: none.

Evidence:

- docs/architecture/spotify.md:161-168 says any operation containing a
  local-file URI fails before HTTP.
- add/remove playlist tracks serialize supplied arrays without that check at
  crates/retune-spotify/src/client.rs:381-409 and :435-461.
- play does the same at client.rs:718-739, and change_library at :804-820.
- Current shells guard several paths, including
  apps/desktop/src-tauri/src/playlists.rs:181-204 and
  spotify_commands.rs:449-478 and :581-600.

Why it matters:

The shared client owns HTTP and is the only boundary that can guarantee zero
requests for invalid mixed URI lists. Caller guards are duplicated and a new
caller can omit them.

Smallest acceptable change:

- Add one private reject_local_uris helper used by every URI-bearing Spotify
  write before request construction.
- Return the existing InvalidRequest variant. No endpoint hierarchy is needed.

Done when:

- Mixed Spotify/file lists for play, playlist add/remove, and library mutation
  return InvalidRequest and record zero fake requests.

### [x] SOLID-022 — Do not report an unavailable Spotify fact as false

Rules: U-L1, U-L2  
Size: S  
Dependencies: SOLID-010 can supply the boundary shape.

Evidence:

- apps/desktop/src-tauri/src/spotify_commands.rs:371-378 logs failure from
  is_following_artist and returns following: false.
- Optional artist enrichment is also silently dropped at
  spotify_commands.rs:625-636.

Why it matters:

The UI states that the user does not follow an artist when Retune simply could
not determine the fact. A follow action can then be offered from an invalid
assumption.

Smallest acceptable change:

- Return the page error, or represent follow state as true, false, or unknown.
- For optional enrichment, explicitly mark/log degraded data if continuing is
  the desired product behavior.

Done when:

- A follow-state read failure renders unavailable/retry behavior and cannot be
  confused with a confirmed false result.

## P2 — structural work at earned seams

### [x] SOLID-023 — Break the lastfm and lastfm_import dependency cycle

Rules: U-S1, U-D1, U-D2  
Size: M  
Dependencies: SOLID-001; do before SOLID-024 and SOLID-026.

Evidence:

- apps/desktop/src-tauri/src/lastfm.rs:196, :573, :744, :847, :857, :1741,
  and :1840-1858 depend on receipt and metadata types defined by lastfm_import.
- lastfm.rs:445 depends on the importer's page-limit constant.
- lastfm.rs:985-1033 reaches through AppHandle to AppState to clear importer
  state and schedule incremental sync.
- Conversely, lastfm_import.rs:4514, :6125, :6285, and :6353 depend on
  lastfm::Service.
- Import persistence reaches through lastfm for atomic file behavior at
  lastfm_import.rs:3029, :3075, :3169, :3261, and :3268.
- Disconnect coordination is in the command at lastfm.rs:1926-1931, while
  finish coordination lives inside Service::finish.

Why it matters:

Auth/scrobbling changes force importer coupling, importer types infect the
connector, and the connector cannot be constructed independently of Tauri
application state. There is no stable inward dependency direction.

Smallest acceptable change:

- Move AcceptedScrobbleReceipt, ScrobbleMetadata, and connector pagination
  constants to the Last.fm connector/model owner or a small neutral model
  module.
- Move account-change clearing and incremental-sync scheduling from
  Service::finish to the existing finish_lastfm application coordinator.
- Use the shared persistence primitive from SOLID-001.
- Establish one direction: importer depends on the Last.fm connector; the
  connector never depends on the importer.

Done when:

- lastfm.rs contains no lastfm_import reference.
- The connector can be constructed and tested without AppState.
- Finish/disconnect orchestration has one application-level owner.

### [x] SOLID-024 — Split the Last.fm importer without changing behavior or schemas

Rules: U-S1, U-S3, U-I1, U-G1  
Size: XL, as a series of mechanical changes  
Dependencies: SOLID-007, SOLID-010, SOLID-011, and SOLID-023 first.

Evidence:

- apps/desktop/src-tauri/src/lastfm_import.rs is 21,640 lines; production code
  runs through about :11365 and tests follow.
- Wire, persisted, and UI models begin at :35.
- Aggregation and review projection occupy the early pure sections through
  roughly :2238; apply planning begins around :2547.
- Three persistence domains begin around :2781, :2967, and :2972.
- Service at :3461-3473 holds three stores, three state mutexes, coordination
  locks, and three runner flags.
- Source download orchestration is around :6166, matching around :8357,
  durable apply around :9489, and Tauri commands from :9890 onward.
- Seven too-many-arguments suppressions at :2218, :2546, :4699, :4767, :4828,
  :5178, and :5341 show recurring data ownership problems, not merely length.

Why it matters:

Persistence migration, Last.fm source policy, matching, review projection,
durable apply, and IPC change for different reasons but share one edit,
compile, review, and merge unit. The current concentration encourages another
local bandage because moving one responsibility safely is expensive.

Smallest acceptable change:

Keep one public Service facade and the current serialized JSON/event contracts
initially. Mechanically split inside the desktop crate along proven sections:

- lastfm_import/model.rs for persisted and boundary types
- lastfm_import/source.rs for recent-track download/aggregation input
- lastfm_import/matching.rs for ranking/classification
- lastfm_import/reconciliation.rs for history/mapping decisions
- lastfm_import/apply.rs for durable apply plans and worker
- lastfm_import/store.rs for session, incremental, mapping, cache, and journal
  persistence, separated internally where useful
- lastfm_import/commands.rs for thin Tauri adapters

Move pure functions with their existing tests first. Narrow facade state only
after moves are stable. Do not create a new crate, move matching into
retune-core, or add a trait per file.

Done when:

- Each module has one nameable reason to change without and.
- Persisted bytes, versions, event names, matching outcomes, and public command
  behavior are unchanged by mechanical split commits.
- No behavior change shares a commit with bulk file movement.

### [x] SOLID-025 — Move importer use cases behind thin Tauri commands

Rules: U-S2, U-D1, U-I1, U-T1  
Size: L, incremental  
Dependencies: SOLID-023; easiest during SOLID-024.

Evidence:

- lastfm_import.rs:9985-10019 performs locking, account resolution, persisted
  review mutation, optional backlog sweep, and event projection in one command.
- Collection search/preview/add/remove at :10248-10476 perform account
  snapshots, remote I/O, identity rechecks, reranking, persistence, and events.
- change_track at :10477-10585 and change_album at :10587-10667 validate batch
  ownership, build queries, call Spotify, classify/preserve candidates, persist,
  and project the UI.
- Repeated username, spotify_account_id, batch_id, artist, and album parameters
  contribute to the too-many-arguments suppressions.

Why it matters:

Commands cannot be tested without Tauri-shaped state and can drift in
lock/account-check/event ordering. Repeated primitive arguments make it easy to
mix identities.

Smallest acceptable change:

- Put each application use case on the importer facade or in an application
  module that returns an outcome/view.
- Commands deserialize, call, emit, and serialize errors.
- Introduce only earned value objects for recurring identity clumps, notably
  AccountBinding and ReviewBatchKey.
- No command-handler framework.

Done when:

- Core review/search/change flows run against fake collaborators without a
  Tauri AppHandle.
- Tauri command bodies contain translation, one use-case call, and event/output
  adaptation only.

### [x] SOLID-026 — Separate Last.fm connector effects from service policy

Rules: U-S1, U-S3, U-D1, U-D2, U-T1  
Size: L  
Dependencies: SOLID-023 first.

Evidence:

- apps/desktop/src-tauri/src/lastfm.rs:566-584 stores credentials, a concrete
  reqwest client, session store, pending-token store, queue store, runtime,
  accepted receipt reconciliation, listening state, an optional AppHandle, and
  test-only request queues.
- Service::new at :587-719 reads ambient build credentials, selects platform
  storage, changes behavior under cfg(test), constructs HTTP, loads/migrates
  queues, and applies account isolation.
- attach_app at :731 creates a second initialization phase.
- post at :1561-1625 returns from a cfg(test) queue before production
  credentials, signature, request, status, and response handling execute.
- emit_state at :1682-1688 silently does nothing before attachment.

Why it matters:

Connector/API, credential storage, queue ledger, listening policy, retry worker,
and UI emission change independently. Tests bypass the most important external
fault boundary instead of substituting only the network.

Smallest acceptable change:

- Pass build credentials into construction from run rather than reading them
  inside Service.
- Inject one narrow Last.fm request executor/transport and an event callback.
  The existing test response queue proves both seams are earned.
- Keep signing, parameter assembly, status mapping, and parsing on the shared
  production path used by both real and fake transports.
- Split internal auth/api, scrobble/listening, and store responsibilities while
  retaining a facade. Keep the justified SessionStore seam.

Done when:

- A fake transport observes api_key, method, sk, format, and api_sig from the
  production request builder.
- Network, HTTP, invalid JSON, and Last.fm API failures execute the release path
  in tests.
- Construction is single-phase and contains no cfg(test) behavior branch.

### [x] SOLID-027 — Put reusable Spotify membership behavior below command adapters

Rules: U-S2, U-D1, U-L1, U-I1  
Size: M to L  
Dependencies: SOLID-003 and SOLID-010.

Evidence:

- OAuth/application use cases occupy
  apps/desktop/src-tauri/src/spotify_commands.rs:77-218.
- Reusable album and track save operations at :449-506 and :581-679 accept all
  of AppState.
- Comments at :447 and :580 require the caller to hold spotify_library_gate,
  but the signature cannot enforce that precondition.
- The importer depends upward on these command-module functions at
  lastfm_import.rs:9341 and :9362.

Why it matters:

A future caller can omit the gate. Shared business behavior is owned by a Tauri
adapter and accepts unrelated state. Remote success followed by local
membership/library persistence failure also needs one recovery contract.

Smallest acceptable change:

- Create a spotify_membership application module of free functions that
  acquires its own gate and accepts only the client, membership owner, library
  owner, and requested operation.
- Commands and importer both call it.
- Preserve documented idempotent retries. Add an interface only if a real fake
  becomes necessary; free functions are sufficient now.

Done when:

- No caller must remember an undocumented lock precondition.
- Importer no longer imports business operations from spotify_commands.
- Remote-success/local-failure retry behavior is covered once at the shared
  owner.

### [x] SOLID-028 — Replace unrestricted Library record mutation with earned methods

Rules: U-S1, U-I1, U-S3, U-T1  
Size: M  
Dependencies: do before large Last.fm file moves.

Evidence:

- TrackRecord identity and mutable facts are public at
  crates/retune-core/src/model.rs:57-95.
- Library::tracks_mut exposes the entire mutable slice at model.rs:162-164.
- Library otherwise explicitly validates duplicate IDs/URIs at :358-381.
- Production consumers need focused operations:
  lastfm_import.rs:2448-2498 merges history,
  localfiles.rs:121-153 fills missing technical metadata, and
  lib.rs:1984-2008 records one play.

Why it matters:

Any shell caller can change ID/URI, create duplicates, bypass overlay edit
semantics, or update history without saturation and monotonic timestamp rules.
The core cannot uphold invariants while exposing every field for mutation.

Smallest acceptable change:

- Add deterministic Library methods for record-play-by-URI, additive/absolute
  history merge, and fill-missing technical metadata.
- Make tracks_mut crate-private or remove it from the cross-crate API.
- Keep filesystem probing and Last.fm parsing in the shell; pass facts inward.

Done when:

- Core tests cover count saturation and min/max timestamp behavior.
- Desktop production code has no direct mutable record slice.
- Compile visibility prevents shell code from changing identity fields.

### [x] SOLID-029 — Shrink lib.rs and stop using AppState as a deep service locator

Rules: U-S1, U-D1, U-D2, U-I1  
Size: L, incremental  
Dependencies: build from SOLID-002, SOLID-003, SOLID-005, and SOLID-027.

Evidence:

- AppState at apps/desktop/src-tauri/src/lib.rs:86-112 has roughly two dozen
  fields across library, Spotify membership/catalog, settings, playlists,
  playback, Last.fm, media keys, and sync.
- library_commands.rs, playback_commands.rs, playlist_commands.rs, and
  spotify_commands.rs begin with use super::*, hiding their dependency lists.
- Playback at playback/mod.rs:1109-1238 and Last.fm at lastfm.rs:1022 retrieve
  arbitrary AppState through AppHandle.
- lib.rs also owns view DTO mapping around :190-716, settings at :736-983,
  Spotify sync at :1087-1755, navigation at :1756-1860, library/local import at
  :1871-2105, menus at :2106-2282, backup at :2283-2549, and run/setup at
  :2555-2924.
- sync.rs imports the pure spotify_track_match helper upward from lib.rs at
  sync.rs:8-12.

Why it matters:

Feature code can acquire unrelated stores and locks, dependency/lock ordering
is difficult to review, and test construction duplicates the entire root.
Composition itself is not the problem; arbitrary reach-through is.

Smallest acceptable change:

- Keep one Tauri managed root and run as the composition root.
- Let concrete owners emerge from the correctness work: LibraryState,
  SettingsState, PlaylistState, and SpotifyState are reasonable groupings only
  where they own a mutation boundary.
- Pass narrow owner references or use-case parameters below commands rather
  than AppHandle/AppState.
- Move pure matching/view/backup/settings/sync helpers to their owning modules.
- Replace glob imports with explicit imports as each module is touched.
- No DI container and no repository per JSON file.

Done when:

- run constructs external clients/stores and wires owners once.
- Deep services no longer fetch arbitrary state through AppHandle.
- lib.rs primarily contains root state, wiring, menus/startup, and command
  registration.

### [x] SOLID-030 — Centralize IPC in small domain gateways

Rules: U-I1, U-D2, U-T1, U-G1  
Size: L, incremental  
Dependencies: SOLID-010; use during frontend fixes rather than as a flag day.

Evidence:

- About 77 Rust commands are registered at
  apps/desktop/src-tauri/src/lib.rs:2561-2639.
- The frontend contains roughly 74 literal invoke sites spread across App.tsx,
  LastFmImporter.tsx, spotifyViews.tsx, and dialogViews.tsx.
- Command names, argument casing, result types, and failure handling are
  manually repeated; TypeScript cannot validate the Rust side.
- Settings persistence and importer theme access demonstrate that several DTOs
  are much wider than their consumers need.

Why it matters:

Command or field drift compiles and fails at runtime. Components reinvent error
conversion, retry, suppression, and result identity, while broad DTOs couple
views to persisted implementation details.

Smallest acceptable change:

- Add small library, playback, Spotify, and Last.fm frontend gateway modules.
  Each owns literal command names, argument/result types, and accepts a fake
  invoker for controller tests.
- Keep DTOs consumer-shaped; apply the settings view/patch from SOLID-002.
- Do not create a generic RPC class hierarchy.
- Cross-language code generation may be evaluated after consolidation if drift
  remains costly; adding a dependency is not required for the first useful
  boundary.

Done when:

- Feature components contain no raw command strings for migrated operations.
- Gateway tests prove exact command names and argument mapping.
- Stale/failure controller tests run with a fake invoker.

### [x] SOLID-031 — Use serde enums for closed command and settings domains

Rules: U-O1, U-I1, U-L1  
Size: S to M  
Dependencies: ReviewAction can land independently; settings modes fit SOLID-002.

Evidence:

- Import review action enters as String at
  apps/desktop/src-tauri/src/lastfm_import.rs:9985-9992.
- It is independently interpreted at :9980, :5179, :5222, and :5305.
- playback_backend and repeat persist as String at
  apps/desktop/src-tauri/src/store.rs:122-124 and are validated at :412-422.
- Their literal behavior is repeated through playback/connect.rs,
  playback/mod.rs, playback/reducer.rs, and TypeScript, which already uses
  PlaybackBackend and RepeatMode unions at apps/desktop/src/types.ts:3-5.

Why it matters:

Invalid internal states remain representable, and adding/renaming a case
requires coordinated literal edits at multiple policy sites.

Smallest acceptable change:

- Introduce a serde ReviewAction enum first, with methods for whether IDs are
  required and whether backlog sweeping is required.
- Convert playback backend and repeat to serde enums while preserving the
  existing serialized strings and defaults.
- Use exhaustive matches. Do not introduce a strategy interface for closed
  sets.

Done when:

- Unknown boundary strings fail deserialization cleanly.
- Adding a variant produces compiler-guided exhaustive-match failures.
- Existing settings files round-trip unchanged.

### [x] SOLID-032 — Reuse the Spotify transport for OAuth token requests

Rules: U-S1, U-D1, U-T1  
Size: M  
Dependencies: stabilize token lifecycle with SOLID-004 first.

Evidence:

- code exchange accepts a concrete reqwest::Client at
  crates/retune-spotify/src/auth.rs:99-141.
- Both desktop OAuth flows construct a fresh raw client at
  apps/desktop/src-tauri/src/spotify_commands.rs:107-115 and :175-183.
- HttpTransport has shared connect/total timeout configuration at
  crates/retune-spotify/src/client.rs:88-100.
- Token refresh already uses injected Transport directly, outside
  send_api_request, at client.rs:952-980.
- docs/architecture/spotify.md:27-31 correctly scopes the request gate to Web
  API requests, while broader wording in AGENTS.md:23-24 and
  ARCHITECTURE.md:201 says all Spotify traffic is shared.

Judgment:

The accounts token endpoint should not share Web API cooldown/request-count
policy. The defect is a concrete, differently configured, non-fakeable HTTP
path—not failure to put OAuth under the Web API gate.

Smallest acceptable change:

- Reuse the existing low-level Transport and one token request builder/parser
  for authorization-code exchange and refresh.
- Keep token endpoint policy outside the Web API request gate.
- Clarify the project wording: Web API requests use the gate; OAuth
  bootstrap/refresh use the shared transport/token coordinator.

Done when:

- Fake-transport tests cover encoded PKCE form fields, success, non-2xx,
  malformed JSON, and transport failure.
- Token exchange does not mutate Web API cooldown or request counts.

### [x] SOLID-033 — Make Spotify catalog generation mean actual change

Rules: U-L1, U-I1, U-T1  
Size: M  
Dependencies: none.

Evidence:

- crates/retune-spotify/src/catalog.rs:59-63 says generation increases when the
  catalog changes and is used to suppress writes.
- Observe/hint operations bump unconditionally, including identical facts, at
  catalog.rs:81-138 and :192-263.
- Nested track/album observation can bump several times for one logical input.
- apps/desktop/src-tauri/src/lib.rs:913-942 uses equality to decide whether to
  rewrite the full catalog.
- SpotifyCatalog.version, v1, and nested maps are public at catalog.rs:15-34,
  while generation is private; future direct mutation can bypass dirty
  tracking.

Why it matters:

Repeated unchanged search/sync observations mark the catalog dirty and cause
redundant whole-file writes. The public representation makes the documented
generation contract unenforceable.

Smallest acceptable change:

- Have merge helpers report whether data changed, aggregate nested changes, and
  bump once when at least one fact changed.
- Privatize v1/maps and add only the read accessors current consumers require.

Done when:

- Identical observation leaves generation unchanged.
- A changed fact advances it once per logical observation.
- Serialization remains compatible.

### [x] SOLID-034 — Key frontend drafts and pending mutations to their entity

Rules: U-S1, U-L1, U-T1  
Size: M  
Dependencies: SOLID-011 and SOLID-013 establish the same identity pattern.

Evidence:

- ImportPage owns a review draft at
  apps/desktop/src/LastFmImporter.tsx:407-424 and resets it whenever the page
  object changes, not only when batch identity changes.
- Genre text persists only on blur at :770, while background import events can
  replace the same page at :1022-1034.
- Spotify membership uses one adding string at
  spotifyViews.tsx:394 and :437-455; another row remains actionable and an
  earlier completion can clear a later pending indicator.
- Add-to-playlist uses one busy string at App.tsx:1328-1362 but disables only
  the matching row.
- Sidebar creation at App.tsx:885-905 lacks an in-flight guard, and concurrent
  rating requests can refresh in completion order.

Why it matters:

Unrelated revalidation can erase unsaved review input. Overlapping operations
show incorrect pending state, allow duplicates, or let an older response appear
to win over the latest user intent.

Smallest acceptable change:

- Reset importer drafts on ReviewBatchKey change, not object identity; merge
  same-batch authoritative updates deliberately.
- Use a Set keyed by URI/playlist ID where independent concurrency is valid.
- Otherwise disable/serialize that small workflow.
- Use a local latest-intent token for ratings.
- Keep these feature-local; no generic global mutation manager.

Done when:

- Same-batch refresh preserves a focused draft.
- Independent rows retain independent pending state.
- Double create/add is blocked or safely serialized.
- Out-of-order ratings honor the latest intent.

### [x] SOLID-035 — Extract a playback effect executor only when this shell changes

Rules: U-S3, U-D1, U-T1  
Size: M to L  
Dependencies: deferred; do not disturb the strong reducer proactively.

Evidence:

- Playback::listen requires AppHandle at
  apps/desktop/src-tauri/src/playback/mod.rs:598-617.
- handle_event at :1120-1292 interprets reducer actions while fetching
  AppState, resolving Spotify, updating media keys, emitting UI events,
  fetching artwork, and scheduling reconnect work.
- media_keys.rs:148-181 and playback_commands.rs:26-49 duplicate portions of
  next/previous/authorization behavior.

Why it matters:

Reducer behavior is well tested, but effect execution requires a Tauri-shaped
environment and duplicate entry paths can drift.

Smallest acceptable change:

- When this area next needs behavior work, extract one narrow action executor
  whose collaborators are emit, provider lookup, media update, artwork, and
  reconnect scheduling.
- Share next/previous use-case functions between commands and media keys.
- Keep PlayerBackend as a closed enum; do not add a backend trait hierarchy.

Done when:

- Reducer action-to-effect mapping can be tested with small fakes.
- Command and media-key entry paths share the same use case.

## P3 — opportunistic and mechanical cleanup

### [x] SOLID-036 — Narrow persistence stores and surface cleanup failures

Rules: U-S1, U-I1, U-L2  
Size: M  
Dependencies: SOLID-001 and SOLID-023.

Evidence:

- FsSyncStore at apps/desktop/src-tauri/src/store.rs:514-518 and :702-740 owns
  cooldowns, artist genres, and Spotify membership, which have different
  consumers and change triggers.
- Import cache cleanup discards filesystem errors at
  lastfm_import.rs:3393-3395; callers at :4165, :4576, :6515, and :6609 can
  still report success.
- Import persistence currently reaches into lastfm for filesystem behavior,
  reinforcing the cycle covered by SOLID-023.

Why it matters:

A consumer that needs cooldowns receives a concrete owner for unrelated
membership state. Silent cleanup failure can retain account listening-history
cache without any diagnostic. This is narrower than the P0 atomic-write defect,
so it should follow the shared primitive.

Smallest acceptable change:

- Split FsSyncStore into narrow concrete stores for cooldowns, artist genres,
  and membership when the owning callers are moved. Do not add traits until a
  second implementation or fake is needed.
- Return cleanup errors. If cleanup remains intentionally nonfatal, log and
  record that policy at the call site rather than silently dropping it.

Done when:

- Callers receive only the concrete store they use.
- Account-sensitive cache cleanup failure is observable and tested.

### [x] SOLID-037 — Remove device playback from the device-free audio crate

Rules: U-S1, U-D2, U-G1  
Size: S  
Dependencies: none.

Evidence:

- crates/retune-audio/src/lib.rs:1 describes probing/decoding without an audio
  device.
- Its rodio usage is Source/sample integration at lib.rs:13 and :188-235.
- crates/retune-audio/Cargo.toml:9 nevertheless enables rodio playback, pulling
  cpal and platform output stacks into the standalone crate.
- apps/desktop/src-tauri/Cargo.toml:43 already enables playback at the desktop
  composition owner.

Why it matters:

The device-free library and its tests compile a native output backend they do
not use, widening platform/build dependencies and responsibility.

Smallest acceptable change:

- Keep rodio default features off and remove playback from retune-audio.
- Leave device playback enabled only in the desktop crate.

Done when:

- cargo tree for retune-audio contains no cpal.
- Audio crate tests and the desktop build still pass.

### [x] SOLID-038 — Test production retry/time policy instead of compiling it away

Rules: U-D1, U-T1  
Size: S to M  
Dependencies: none.

Evidence:

- crates/retune-spotify/src/client.rs:19-22 compiles production retry delays of
  one and three seconds to zero under tests.
- Tests at client.rs:1825-1856 assert request count/status but do not exercise
  the real schedule.
- expiry and HTTP-date Retry-After decisions read ambient wall time at
  client.rs:282-287, :981-986, and :1008-1029.

Why it matters:

The production timing branch is not executed, and boundary decisions cannot be
tested deterministically. This is modest risk and does not justify a global
Clock abstraction.

Smallest acceptable change:

- Use production constants with Tokio's paused clock.
- Pass now into the small pure expiry/header helpers, or inject a tiny wall-time
  function locally.
- No clock trait hierarchy.

Done when:

- Two server retries advance virtual time by exactly four seconds.
- Expiry and HTTP-date behavior are covered before, at, and after the boundary.

### [x] SOLID-039 — Split Spotify client source after behavior is stable

Rules: U-S1, U-G1  
Size: L, mechanical  
Dependencies: SOLID-004, SOLID-021, SOLID-032, SOLID-033, and SOLID-038 first.

Evidence:

- crates/retune-spotify/src/client.rs is 2,585 lines.
- Transport and fake transport occupy roughly :24-210.
- Client construction, request policy, refresh, and all endpoint families
  occupy :212-991.
- Helpers and wire DTOs occupy :993-1427, followed by tests through :2585.

Why it matters:

Transport/retry/token policy, endpoint contracts, wire models, and their tests
have different reasons to change and compete in one source unit. Unlike the
Last.fm importer, this module already has a coherent public facade, so only a
source split is warranted.

Smallest acceptable change:

- Retain one public SpotifyClient, one request gate, and the existing Transport
  seam.
- Mechanically move transport, request policy, wire models, and
  endpoint-oriented implementation sections into cohesive source modules.
- Do not introduce service/factory layers.

Done when:

- Public API and behavior are unchanged.
- File moves are reviewable separately from behavior changes.

### [x] SOLID-040 — Restore one owner for architecture documentation

Rules: U-S1, U-T2  
Size: S  
Dependencies: update alongside the first affected behavior change.

Evidence:

- docs/DEVELOPMENT.md:150-153 defines ARCHITECTURE.md as the system map and
  domain documents as current behavior truth.
- ARCHITECTURE.md:131-132 says cached Last.fm batches have no adjacent
  prefetch.
- docs/architecture/lastfm-import-matching.md:106-109,
  docs/architecture/spotify.md:147-150, and
  apps/desktop/src/LastFmImporter.tsx:1039-1049 define and implement one-batch
  lookahead.
- AGENTS.md:23-24 and ARCHITECTURE.md:201 broadly say all Spotify requests use
  the shared client/gate, while docs/architecture/spotify.md:27-31 correctly
  scopes the gate to Web API requests; OAuth token transport is a separate
  concern in SOLID-032.

Why it matters:

Duplicated procedural policy has already contradicted its owning domain
document. Reviewers and future changes cannot tell which statement is
authoritative.

Smallest acceptable change:

- Correct the stale prefetch sentence.
- Keep ARCHITECTURE.md to topology, ownership, and cross-domain invariants.
- Link to the owning domain document for detailed Last.fm and Spotify policy.
- Clarify shared Spotify wording as described in SOLID-032.

Done when:

- Each behavioral rule has one owning document.
- The system map does not restate change-prone procedural details.

## Historical recommended delivery sequence

This order minimizes rework and keeps each change reversible.

### Phase 0 — Tighten the cheap ratchets

1. SOLID-017: fix the seven hook warnings, enable strict after verification,
   and run frontend test/lint in CI.
2. SOLID-007: repair importer failure-as-success with direct regression tests.
3. SOLID-015, SOLID-021, and SOLID-022: land the small error/validation
   boundary fixes.
4. Start deleting only the source-regex assertions replaced by those behavior
   tests under SOLID-018.

### Phase 1 — Repair durable mutation ownership

1. SOLID-001: one unique-temp atomic file primitive.
2. SOLID-002: one settings mutation owner and narrow patches.
3. SOLID-003: one playlist mutation gate.
4. SOLID-004 and SOLID-016: token lifecycle atomicity and store conformance.
5. SOLID-005: sync on a working copy with one durable commit.
6. SOLID-006: multi-component restore journal/recovery.
7. SOLID-009: late audio failure signaling.

These changes should use failure injection and controlled interleavings before
any large file is moved.

### Phase 2 — Fix frontend async identity

1. SOLID-008: key playlist data.
2. SOLID-011 and SOLID-012: importer refresh and playback intent generations.
3. SOLID-013: key Spotify/info results per view.
4. SOLID-014: delete redundant global caches.
5. SOLID-019 and SOLID-034: repair interaction, draft, and pending-operation
   ownership.
6. Replace the corresponding source-text tests under SOLID-018 as each
   behavior becomes executable.

### Phase 3 — Stabilize application contracts

1. SOLID-010: preserve typed errors across apply and playlist policy.
2. SOLID-020: make the provider own a complete sync run.
3. SOLID-028: close unrestricted core mutation.
4. SOLID-030 and SOLID-031: migrate IPC through domain gateways and closed
   enums.
5. SOLID-032 and SOLID-033: unify token transport and make catalog generation
   truthful.
6. SOLID-027: put reusable Spotify membership behavior below commands.

### Phase 4 — Untangle Last.fm

1. SOLID-023: remove the connector/importer cycle.
2. SOLID-026: inject Last.fm transport/events and remove test-only behavior.
3. SOLID-025: move application use cases behind thin commands.
4. SOLID-024: mechanically split the importer, pure sections and tests first.

No step in this phase should change persisted formats merely to make the split
look cleaner.

### Phase 5 — Consolidate composition and source layout

1. SOLID-029: shrink lib.rs and group only the mutation owners already proven
   useful.
2. SOLID-035: improve playback effect execution only when playback behavior is
   next touched.
3. SOLID-036 through SOLID-040: narrow stores, remove the audio feature leak,
   test time policy, split Spotify client source, and repair doc ownership.

## Historical change and review discipline

Every remediation change should follow these rules:

1. A behavior defect gets the smallest failing regression first or in the same
   change.
2. A mechanical module move does not also change behavior.
3. Keep persisted versions, JSON field names, Tauri command/event names, and
   public APIs stable during decomposition unless changing that contract is the
   explicit ticket.
4. Fix at the owner shared by all callers. Do not add guards independently to
   every caller when one boundary can enforce the rule.
5. Add a trait/interface only when there is a real alternative implementation
   or fake. Prefer parameters, closures, enums, and free functions otherwise.
6. New public surface must have a real production consumer. Remove test-only
   production helpers.
7. Update the owning architecture document in the same change when a boundary,
   invariant, persisted format, or external contract changes.
8. Run the complete checks in docs/DEVELOPMENT.md before merging.

## Explicit non-goals

The following work would add complexity without addressing a finding:

- no DI container or service locator framework;
- no repository interface per JSON file;
- no crate per Last.fm or Spotify subfeature;
- no strategy hierarchy for closed playback, repeat, or review-action enums;
- no migration of Last.fm matching into retune-core;
- no replacement of the playback reducer/backend event model;
- no global Clock trait;
- no generic frontend request or mutation manager;
- no mandatory IPC code-generation dependency before small gateway modules
  prove insufficient;
- no cache rewrite without measured latency after redundant caches are removed;
- no bulk React component splitting based only on line count;
- no blanket ban on Result values containing messages when callers do not make
  policy decisions from their kind.

## Completion criteria used

The codebase is in practical alignment with this audit when:

- each durable aggregate has one mutation/commit owner;
- account replacement and disconnect cannot be overwritten by stale work;
- live state changes only after its durable commit succeeds, or an explicit
  recovery journal makes partial application recoverable;
- policy never branches on user-facing error prose;
- Tauri commands are adapters over testable use cases;
- retune-core owns deterministic model mutations without exposing unrestricted
  identity-bearing records;
- async frontend state is keyed to request, entity, and account identity;
- rendered interaction behavior and important failure paths execute in tests;
- frontend tests/lint are enforced by CI with no warning loophole;
- the Last.fm importer is split along its real source, matching,
  reconciliation, apply, persistence, and command seams without a big-bang
  rewrite;
- architecture details have one documented owner;
- the strengths and non-goals above remain intact.
