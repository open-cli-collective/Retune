# Rust slop and performance audit — 2026-09-05

Audited revision: `bdfc933`. Evidence comes from current source, caller searches,
executed tests, and the accompanying benchmark. Earlier audit records and
architecture claims are not evidence that a problem is fixed.

The highest-value work is removing repeated whole-library lookups. There is also
test instrumentation that claims to establish performance without measuring the
expensive work. I did not find evidence justifying a wholesale rewrite, removal
of the playback reducer, or removal of the persistence/account guards.

This pass inventories the four Rust crates and traces the affected browse,
playlist, playback preparation, startup, import review, and persistence flows.
It is a slop/performance audit, not an exhaustive correctness, security, native
audio, or cross-platform certification. Production code is unchanged. The only
additions are this report and a runnable CPU benchmark.

**Measured evidence.** Release build, Apple M4 Pro, arm64, Rust 1.98.0.
Each number is a median of five runs after one warmup; input construction is
outside timing. The dataset has unique track URIs, 500 artists, 5,000 album
labels, mixed-case categories, populated metadata, and explicit/inherited/unrated
tracks. Experiment outputs are checked against the existing outputs.

| Operation | Current, 10k tracks | Current, 20k | Current, 50k | Experiment, 50k |
| --- | ---: | ---: | ---: | ---: |
| Ratings for every track | 27.952 ms | 116.358 ms | 708.215 ms | 2.327 ms |
| Queue URI lookup kernel | 84.268 ms | 320.713 ms | 2,080.714 ms | 1.773 ms |
| Metadata backfill with nothing missing | 27.299 ms | 110.265 ms | 727.942 ms | Not implemented |
| Unfiltered browse facets | 11.650 ms | 32.509 ms | 81.160 ms | 7.886 ms |
| Library JSON load | 21.111 ms | 42.791 ms | 106.279 ms | 73.451 ms |

These are CPU measurements, not full UI/startup timings or shipped speedups.
Queue measurements reproduce the library-hit lookup kernel, include building the
experimental index, and exclude queue DTO construction, the later enabled-track
pass, playlist/catalog snapshots, IPC, and network calls. Backfill measures the
existing core mutation calls on already-populated records; it excludes filesystem
probes, library cloning, serialization, and disk writes. JSON measurements include
parsing, library validation, and output destruction. No personal library or live
service was accessed.

Run from the repository root:

```sh
rtk cargo run -p retune-core --release --example audit_hotpaths
```

The [benchmark](../crates/retune-core/examples/audit_hotpaths.rs)
is deliberately opt-in and has no new dependency or wall-clock CI threshold.
Its experimental facet implementation covers the generated all-Music, unfiltered
case; it is not a replacement for the production filtering contract.

1. **[P1] Repeated rating lookup makes browse and playlist projection quadratic.**

   [Browse mapping](../apps/desktop/src-tauri/src/library_commands.rs#L550)
   already has `&TrackRecord`, then calls
   [effective_rating](../crates/retune-core/src/model.rs#L483),
   which calls the linear `Library::get`. Displaying all N tracks therefore
   performs N(N+1)/2 ID comparisons. The
   [playlist projection](../apps/desktop/src-tauri/src/playlist_commands.rs#L229)
   repeats this scan after successfully looking up the track in its URI map.
   Spotify album projection also calls the same rating function on an already
   resolved track at
   [spotify_commands.rs](../apps/desktop/src-tauri/src/spotify_commands.rs#L1157).

   Smallest repair: let the core rating owner calculate the rating from a borrowed
   record and use it at these callers. Keep the ID-taking entry point where it is
   useful. No permanent library index is needed for this fix. The experiment
   reduces the 50k rating pass from 708 ms to 2.3 ms. Preserve explicit precedence,
   album inheritance, reparenting, and unrated behavior in a compact regression;
   exercise diverse IDs through the real projection.

2. **[P1] Starting a large queue repeatedly scans the library and playlists.**

   [resolve_cached](../apps/desktop/src-tauri/src/playback_resources.rs#L16)
   invokes `resolve_one` for each requested resource. That function scans all
   library tracks and, on a miss, all cached playlist tracks. Then
   [resolve_resources](../apps/desktop/src-tauri/src/playback_commands.rs#L165)
   scans the library again for each enabled check. `finish` can perform that check
   twice for a row before the starting position. These loops run inline in the
   async command; `play_tracks` also clones the whole library, playlist cache, and
   catalog before resolving even a single cached track.

   Smallest repair: build transient URI lookup maps once for the queue operation,
   reuse them for resolution and enabled checks, and keep costly CPU preparation
   off the async executor. Preserve first-playlist-match precedence, queue order,
   duplicates, URI-based local membership validation, and the explicitly selected
   disabled track. The index experiment costs 1.8 ms including construction versus
   2.08 seconds for just the 50k library-hit scan. Snapshot narrowing can follow
   measurement; do not replace snapshots with long-lived live-state locks.

3. **[P2] Startup backfill does quadratic work and saves on every no-op run.**

   [backfill_metadata](../apps/desktop/src-tauri/src/localfiles.rs#L122)
   collects an update for every track, even if all fields are present, and calls
   `fill_missing_metadata` separately for every ID. Each call rescans the vector.
   [Startup](../apps/desktop/src-tauri/src/lib.rs#L1313)
   invokes this through
   [LibraryOwner::mutate](../apps/desktop/src-tauri/src/library_state.rs#L222),
   which clones, serializes, and saves regardless of the returned `false`. That
   holds the mutation gates while doing approximately 728 ms of unnecessary CPU
   work at 50k, plus the unmeasured copy and disk costs.

   Smallest repair: apply missing-field updates through one bulk core operation,
   and skip unchanged saves within the existing serialized mutation boundary.
   Filtering out complete records alone fixes repeat-startup cost but leaves the
   first large backfill quadratic. The same missing bulk boundary affects
   [multi-track Get Info](../apps/desktop/src-tauri/src/library_commands.rs#L396):
   validation and editing independently search by ID for every selected track.
   That edit path was traced but not timed. Retain tests for unknown-ID atomicity,
   fill-only semantics, concurrent edits, and save failure; add a no-op-save check.

4. **[P2] Facet generation allocates and lowercases duplicate strings during sorting.**

   [facets and sorted_unique](../crates/retune-core/src/browse.rs#L64)
   clone category, artist, and album values per track, then lowercase both sides
   inside the comparator before deduplicating. The same artist/album text is
   allocated and normalized repeatedly. The track-ordering function already uses
   `sort_by_cached_key`, but the facet helper does not.

   Smallest repair: collect borrowed strings, deduplicate exact values, and cache
   lowercase keys for the remaining sort. Own strings only for the result.
   The unfiltered 50k experiment falls from 81.2 ms to 7.9 ms. Keep exact-case
   distinct labels, deterministic case-insensitive ordering, the raw-string tie
   breaker, category pinning, and broader-selection filtering.

5. **[P2, slop] Several performance tests measure their own counters, not complexity.**

   [PROJECTION_LOOKUPS](../apps/desktop/src-tauri/src/playlist_commands.rs#L197)
   adds exactly two per row. Its
   [test](../apps/desktop/src-tauri/src/playlist_commands.rs#L306)
   asserts that arithmetic while rendering track 42 ten thousand times. It misses
   finding 1's nested scan and systematically chooses a cheap early-library hit.
   This test passed in the current workspace run.

   The core
   [counted bulk helpers](../crates/retune-core/src/model.rs#L237)
   similarly return manually incremented operation counts through three wrappers
   and test/non-test branches. Those counts do not measure comparisons inside a
   called function. The
   [large browse test](../crates/retune-core/src/browse.rs#L385)
   additionally creates its input using repeated `Library::add`, making test setup
   quadratic, then repeats a sort-key-count property already covered by the small
   adjacent test. Running that single test took 10.37 seconds (test execution,
   excluding compilation). The importer
   [pagination test](../apps/desktop/src-tauri/src/lastfm_import/integration_tests.rs#L4050)
   checks output order and bounds, not its name's claim about materialization.

   `delete:` remove the fixed per-row counters, production `*_counted` plumbing,
   and redundant scale-only assertions. Keep bulk-versus-sequential equivalence,
   ordering, deduplication, and failure tests. Keep large-input measurements in an
   opt-in benchmark that exercises the actual projections, and build fixtures via
   `add_all`. A helper's own counter is not a substitute for measuring its caller.

6. **[P2, shrink] Last.fm collection previews recompute identical match work.**

   [collection_track_statuses](../apps/desktop/src-tauri/src/lastfm_import/review.rs#L410)
   loops candidate track URI × eligible source row × selected track. The exact
   matching-URI set for a row is rebuilt inside the outer URI loop even though
   it does not depend on that URI. The comparison function also normalizes strings
   on each call. In
   [collection_match_view](../apps/desktop/src-tauri/src/lastfm_import/review.rs#L546),
   the baseline status totals are calculated once, then recalculated for every
   cached candidate. Selected albums get full preview calculations once for the
   selected-album summary, throwing away their status vectors, and again for the
   preview list.

   Smallest repair: reuse the existing baseline totals, compute each album preview
   once, and compute each row's exact-match set once per candidate union. This is
   redundant CPU work established from code; no runtime speedup is claimed yet.
   Measure a large unresolved collection before and after. Preserve explicit
   choices, ambiguous matches, marginal coverage, and selected-album order.

7. **[P2, shrink] Import pagination clones all batches before selecting a page.**

   [queue_page](../apps/desktop/src-tauri/src/lastfm_import/service.rs#L1868)
   receives a borrow-capable batch projection, then immediately clones every
   visible `ImportBatch`, including all its source ID strings, before taking
   `cursor..end`. It also scans all source rows per page. The single-page
   [page projection](../apps/desktop/src-tauri/src/lastfm_import/service.rs#L1925)
   clones all batches too. The existing 23,132-batch test passes despite this.

   Smallest repair: retain borrowed batch references and only construct owned
   output for the requested page. Preserve queued/running-job hiding, failure
   visibility, stable page numbers, and invalid-cursor behavior. The full row scan
   remains a separate measured follow-up; do not introduce an invalidated global
   cache merely to eliminate these unnecessary clones. This finding is not timed.

8. **[P2, shrink] Owned data is copied immediately before being consumed.**

   [Core JSON import](../crates/retune-core/src/io.rs#L39)
   owns the envelope but clones its entire `library` JSON subtree before passing
   it to `from_value`. Using `as_object_mut()` and `remove("library")` keeps the
   same validation/error sequence and moves the subtree instead. This applies to
   normal library startup through
   [FsOverlayStore::load](../apps/desktop/src-tauri/src/store.rs#L1564).
   The experiment reduces 50k parsing/validation from 106.3 ms to 73.5 ms and avoids
   the duplicate JSON tree. Peak RSS was not measured.

   Two other concrete move opportunities are
   [import session save](../apps/desktop/src-tauri/src/lastfm_import/service.rs#L860)
   (`next.clone()`) and
   [playlist commit](../apps/desktop/src-tauri/src/playlist_state.rs#L134)
   (`let saved = next.clone()`). Move the candidate into the blocking save task,
   return it on success, then publish it. Retain the outer owned completion and
   mutation guards so cancellation cannot separate disk from memory. These two
   handoff changes are not timed. Do not remove the separate before-state snapshots
   required for comparison, isolation, or recovery. Re-run cancellation/save-failure
   tests and core malformed-envelope/version/duplicate-ID tests when implementing.

9. **[P3, delete] One exported catalog wrapper has no workspace caller.**

   [SpotifyClient::clear_catalog](../crates/retune-spotify/src/client/request.rs#L44)
   has only its declaration in the repository. Remove the seven-line method;
   replacement: nothing. Account cleanup has its own shell-owned flow. This is
   source/API cleanup, not a demonstrated binary-size improvement. No dependency
   removal was established; `cargo machete` was unavailable locally.

**Verification and repair order.** The baseline workspace run passed 861 tests
with one ignored test across 12 suites. The benchmark ran in release mode and
its output-equivalence assertions passed. `cargo fmt --all --check` and
`cargo clippy --workspace --all-targets -- -D warnings` both passed. Tests passing establishes the
tested behavior, not the absence of these performance defects.

Implement ratings and queue lookup first, then backfill/no-op persistence, facets,
and moving the JSON subtree. Replace the misleading performance assertions while
retaining useful behavioral coverage. Profile Last.fm collection projections
before a wider importer cleanup. No API-rate policy, concurrency limit, persisted
schema, security mechanism, or external service contract needs to change for the
measured wins.

Cleanup-only estimate, excluding the opt-in benchmark and any implementation:
net: approximately -90 lines, -0 dependencies possible. This is an estimate of
counter/wrapper/test deletion, not a claim that production code has been changed.
