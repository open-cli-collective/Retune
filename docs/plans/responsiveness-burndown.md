# Responsiveness burndown

Issue: [#67](https://github.com/open-cli-collective/Retune/issues/67)

Branch: `codex/responsiveness-burndown`

Baseline revision: `4312e1d` (tree-identical to audited `bdfc933`)

Fixture/results contract: `responsiveness-v1`

## Outcome

Retune should acknowledge ordinary input without waiting for disk or network,
avoid repeated whole-library/import-session work, keep unrelated navigation
usable during background work, and retain account, ordering, recovery, and
external-write safety. Source and executed checks are evidence; prior prose is
only a finding inventory.

Reference Mac targets are admission p95 at most 5 ms independent of I/O,
typing p95 at most 33 ms with no attributable task over 50 ms, warm 50k browse
median at most 100 ms, and admitted local-search results p95 at most 200 ms.
Coalescing delay, backend completion, and native input-to-paint are reported
separately. These are targets to prove, not current claims.

## Evidence contract

- Fixtures and baseline output use the versioned `responsiveness-v1` contract in
  [responsiveness-baseline-v1.json](../performance/responsiveness-baseline-v1.json).
  Library fixtures cover 10k and 50k tracks. Import fixtures cover 100, 1,000,
  and 5,000 batches; mounted UI fixtures cover up to 1,000 visible collection
  rows, a 50k queue, and 20k suggestions.
- Production operations are measured directly. Copied audit kernels remain
  exploratory evidence and cannot prove a regression or shipped improvement.
- Timed setup stays outside samples and every runner checks result correctness.
  Optimized runs warm once, then record sample count, median, and min/max spread.
- Deterministic request counts, write counts, supersession, and held-I/O tests
  land before timing gates. Native WebKit paint is distinct from jsdom/React
  timing. The native baseline remains open until an isolated app identifier and
  product name protect the user's real Retune data.
- A later CI comparator will use the same optimized runner/toolchain and fixtures
  for the approved baseline and candidate. It fails only after a second paired
  run confirms a regression over 20%, over 2 ms, and beyond measured noise.
  Missing or unstable evidence is inconclusive, never a pass. A reviewed metric
  ratchet commit records accepted gains, and the comparator must reject a
  deliberately slowed case.

## Checkpoints

Each checkpoint ends with focused checks, a diff/evidence review, and one commit.
The completed plan is deleted after durable decisions and evidence move into the
owning architecture documents.

### 0. Preserve the audit and baseline

- Commit the three audit reports and their opt-in Rust/React diagnostics without
  production changes.
- Preserve machine-readable baseline results, fixture definitions, revision,
  build mode, tool versions, sample counts, medians, and spread.
- Open one GitHub issue titled “Eliminate UI stalls and prevent performance
  regressions,” push this branch, and open one draft pull request.
- Record the native-baseline gap explicitly. Do not touch the user's installed
  app data or claim screenshots/jsdom as paint timing.

### 1. Remove independent Rust hot paths

- Compute effective rating from an already-resolved canonical track record in
  core and update browse, playlist, and Spotify projections. Preserve explicit,
  inherited, reparented, and unrated behavior; playlist/catalog callers retain
  their resource identity.
- Build transient URI indexes once per queue operation and reuse them for
  resolution and enabled checks. Preserve queue order, duplicates, the first
  playlist match, and an explicitly selected disabled track.
- Add one bulk fill/edit core boundary for startup metadata and multi-track Get
  Info. Preserve unknown-ID atomicity and fill-only behavior; skip no-op saves
  inside the existing serialized owner boundary without bypassing restoration
  or mutation gates.
- Deduplicate borrowed facet strings, cache normalized sort keys, preserve exact
  case, raw-string ties, category pinning, and the narrowest selected-filter
  order. Move the owned library JSON subtree instead of cloning it.
- Move save-handoff values where ownership already transfers while retaining the
  outer completion/cancellation guards. Delete fixed performance counters,
  redundant scale-only setup, and the unused `clear_catalog` wrapper while
  retaining behavioral coverage.
- Benchmark actual production operations and add focused regression checks for
  ratings, URI resolution, bulk edits/backfill, facets, malformed JSON, save
  failure, cancellation, and no-op persistence.

### 2. Make typing and keyboard work local

- Keep the importer genre draft inside the field and commit on deliberate
  boundaries, preserving refresh behavior and autocomplete semantics.
- Display the main query immediately but admit only the latest pending local
  browse. Spotify typing must not trigger local browse. Keep usable prior results,
  a pending state, stable action identity, and late-response rejection.
- Isolate import queue filter state from review reconciliation; add reusable
  normalized search data only if native measurement justifies it. Make
  `genre_values` collect genres only.
- Give focused rows and facets explicit prefix handlers with stable candidates
  while requests are pending. Preserve IME composition, modifiers, native
  control selection, importer shortcuts, timeout, and mouse parity.
- Prove request counts, supersession, drafts, prefix behavior, and timeout.
  Preserve the fast single-item Get Info path unless native evidence identifies
  it. Measure 50k TrackList input-to-paint before considering reuse of the
  existing queue virtualization pattern.

### 3. Extract importer transformations without changing behavior

- First characterize the existing facade boundary, persisted records, command
  results, ordering, account isolation, journal recovery, and cancellation.
- Extract pure review transformations and borrowed page/queue/collection
  projections. The stateful service remains the owner of locks, publication,
  persistence, and recovery; matching/apply coordinators remain narrow. Keep the
  facade and add no unrestricted helper type or one-implementation interface.
- Reuse the existing pure portion of the importer tests and add focused
  no-filesystem/no-provider tests. Update the owning architecture document.

### 4. Narrow read work

- Replace metadata-only full-session snapshots with owner-phase reads and select
  borrowed batches before constructing owned DTOs.
- Reuse collection baselines, exact-match sets, and previews. Separate prepared
  reads into fetch/rerank/save phases.
- Add only revision lookups proven necessary, with invalidation for candidates,
  mappings, membership, review, and account changes.
- Show local source rows while cold matching runs. Use one reconciliation path
  per action: returned data or an event, not both. Avoid repeated whole-queue
  loads and keep display preferences out of review processing.
- Prove prepared reads perform zero writes/provider calls and verify invalidation
  and UI IPC counts, including an expired-cooldown read that must not prune and
  persist as a side effect.

### 5. Acknowledge drafts immediately

- Define a typed patch carrying session/account generation, target, and changed
  fields. Validate cheaply, admit in service order, and acknowledge before disk
  I/O or full-session cloning.
- Let the service-owned writer coalesce compatible field edits while preserving
  order around structural changes and Apply. Materialize persistence snapshots
  outside the admission lock and prevent older writes from replacing newer
  memory. Whole-options overlap cannot clobber unrelated fields.
- Show pending intent immediately, keep mouse/keyboard admission identical, keep
  unrelated queue/sort/filter/navigation usable, and surface dirty failures with
  retry. Preserve global count-mode meaning; run ignore backlog work with visible
  progress.
- Test held saves with repeated and different-field edits, navigation and Apply,
  stale accounts, retry/failure, window close, and new edits while an old save is
  held. Accepted unflushed drafts may be lost on crash; Apply/recovery stays
  durable.

### 6. Isolate foreground work

- Keep prepared reads and draft admission independent from network and held
  membership locks while retaining required account serialization for external
  writes. Never issue a request under one account and validate it afterward as
  another.
- Apply freezes all earlier drafts into a versioned durable job before external
  effects. Accept & Next exposes the next usable page with a loading state;
  Accept All prepares a dialog immediately, then shows cancellable progress and
  confirmation. Unrelated navigation remains usable during bulk apply.
- Deduplicate matching and prioritize foreground over prefetch without a global
  network lock. Guard search-preview generations so late work cannot reopen UI.
  Return manual album-search summaries first and hydrate on demand without
  degrading automatic match quality.
- Before Spotify behavior changes, verify the current official contract. Keep
  the shared client/request gate and necessary external-write account ordering.
  Test held providers, account switches, retries, durable ordering, and late
  generations.

### 7. Re-audit and ratchet evidence

- Re-run the complete source audit, optimized benchmarks, deterministic checks,
  and isolated native journeys. Address candidate payload duplication, caches,
  bulk editing, or render virtualization only where measurements require it.
- Classify every finding as fixed with executed evidence, disproved, or deferred
  with explicit impact. Required unresolved outcomes prevent completion.
- If a persisted format changes, isolate it with migration and recovery tests.
  Update owning architecture documents and delete this completed plan.
- Finish with formatting, strict Clippy, workspace tests, docs/release/ACL checks,
  desktop tests/lint/build, and existing native CI. No live credentials or audio
  device are required.

## Progress ledger

| Checkpoint | Status | Evidence |
| --- | --- | --- |
| 0 Audit/baseline | Complete | Issue #67, commit `ecd4185`, draft PR #68; native gate remains open |
| 1 Rust hot paths | Complete | [Phase 1 results](../performance/responsiveness-phase1-v1.json) |
| 2 Typing/keyboard | Complete | [Phase 2 results](../performance/responsiveness-phase2-v1.json); native/full-App render gate remains open |
| 3 Importer extraction | Pending | — |
| 4 Narrow reads | Pending | — |
| 5 Draft acknowledgement | Pending | — |
| 6 Foreground isolation | Pending | — |
| 7 Re-audit/ratchet | Pending | — |
