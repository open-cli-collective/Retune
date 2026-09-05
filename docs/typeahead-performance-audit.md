# Type-ahead performance audit

2026-09-05, baseline `bdfc933`. Follow-up to the [Last.fm interaction audit](lastfm-interaction-audit.md) and [Rust audit](rust-slop-performance-audit.md). Evidence comes from source and executed diagnostic cases, not architecture-document claims. No production fixes were applied.

## Findings by typing surface

| Surface | What each character actually does | Finding |
| --- | --- | --- |
| Last.fm Genre autocomplete | Shared `AutocompleteInput` scans cached suggestions, then `setReview` updates the owning `ImportPage`. The page projects decisions, selections, required matches, sorting/partitioning, summaries, and renders every mapping row. | **Measured React work scales with the review pane.** There is no IPC during ordinary typing; save happens on blur. The field's state ownership couples typing to unrelated review work. |
| Main Get Info Artist/Album/Genre | Same autocomplete algorithm, updating the smaller dialog's draft. Suggestions load once on mount. | **Negative result for a universal autocomplete bottleneck:** the measured single-item dialog was fast even with 20,000 artist suggestions. This does not establish the performance of bulk-edit dialogs or actual native text input. |
| Main bulk information autocomplete | Same input; rerenders `MultipleItemInformation`, recomputes `overlayEditTargets(tracks)` and mixed-value placeholders over the selection. | Additional size-dependent work exists in source. **User-visible latency unmeasured**; test this with the actual selected-item count before assuming it needs a new suggestion index. |
| Last.fm queue filter | Every edit normalizes query terms and every scalar field of every loaded queue item. Root state changes also render descendants. An effect copies queue position into a new page object, which can trigger review-draft reconciliation. | Queue virtualization bounds the list DOM, but not filtering CPU or review-pane rerenders. Isolated filtering of 50,000 queue items took approximately 25 ms on this machine. |
| Last.fm matching search inputs | Query changes stay local to the picker/dialog. Search only on explicit Search/Enter. Genre is the autocomplete field; these search boxes do not issue a request per character. | Good request discipline. The collection dialog still rerenders its loaded result/preview content while typing; its frame cost was not measured. |
| Main Library search | Dispatch query at app root; the browse effect immediately invokes `browse`. Old track results become unavailable until the request key matches. Every separate keystroke can start another browse. | **Executed:** three characters produced three requests, and old track rows disappeared while responses were held. Stale results are ignored, but already-started Rust work is not cancelled/coalesced. |
| Main Spotify search | Same query-dependent library browse effect still executes, with **no query**, plus a separate 300 ms timer for remote Spotify search. | **Executed:** three characters in Spotify scope produced three unfiltered library browse calls. A remote-search debounce does not protect this independent expensive local path. This occurs even while disconnected. |
| Main Spotify result display | Query reducer clears results on every character; `SpotifySearch` is keyed by query and its searching state replaces results with a stub. | Results reset/remount instead of remaining visible while the next query resolves. This is a source-confirmed contributor to perceived churn; native paint cost is unmeasured. Started remote requests are ignored on supersession, not aborted by the UI. |
| Main track-list letter jumps | Global prefix handler can scan `displayedTracks`, select a row and rerender the list. However, `routeGlobalShortcut` excludes `[data-keyboard-row]`, and the row handler supports navigation/Enter/Space but not letters. | **Executed correctness issue:** typing a matching letter on a focused track row did not select it. This can feel like latency but is ignored input. Noninteractive-target/global prefix handling remains a separate path. |
| Main facet letter jumps | Global prefix handler scans current `view` facets, immediately selects a match, and starts a browse. `view` becomes null until the new request resolves, although `BrowserPane` continues displaying `state.view`'s old facets. | **Executed:** the next character did not refine selection or start another browse while the first response was held. The buffer advances, but is not replayed on response. Focused facet buttons also fall under the global native-control guard and have no custom letter-prefix handler. |
| Last.fm queue/mapping letter keys | Explicit navigation/actions (`E`, `X`, `S`, `A`, etc.), not generic prefix navigation. | Do not add universal letter jumps here without preserving those existing shortcuts. The queue's text filter is its typing-based search surface. |

## Measurements

### React input/render baseline

Named tools: Vitest 4.1.11, React `Profiler`, jsdom 30.0.1, `performance.now`. Actual components are mounted; IPC is mocked with deterministic fixtures. Tests run on this Mac without CPU throttling. Six successive insertions, first discarded as warmup; five samples summarized. Typed prefixes deliberately do not match a suggestion, exercising full suggestion scans. No live account or native WebKit renderer is involved.

| Fixture | Metric | Median | Min–max |
| --- | --- | ---: | ---: |
| Importer, 25 visible rows, 200 genre suggestions | React render time per insertion | 3.88 ms | 3.33–4.61 ms |
| Importer, 250 visible rows, same suggestions | React render time per insertion | 8.69 ms | 8.47–34.18 ms |
| Importer, 1,000 visible rows, same suggestions | React render time per insertion | 34.72 ms | 32.33–36.52 ms |
| Single-item Get Info, 20,000 artist suggestions | Event dispatch through committed DOM update | 0.63 ms | 0.56–2.22 ms |
| Same Get Info fixture | React render time | 0.07 ms | Median only retained |

Importer fixture rows are unmatched source tracks, with full review controls; these results do not model all candidate-rich collections. Both fields issue **zero per-character IPC calls**. Main Get Info loads suggestions once; importer uses the separate `genre_values` command on mount. Times include test/development-render overhead and are **not input-to-paint or production INP measurements**. The 250-row outlier is retained rather than discarded. Shared workstation load/GC was not controlled; scaling is useful evidence, exact native latency is unverified.

Separately, Node's `performance.now` measured the real `filterImportQueue` helper with synthetic scalar queue entries and query `artist` (all results match), one warmup plus five runs:

| Queue entries | Median | Min–max |
| ---: | ---: | ---: |
| 1,000 | 0.62 ms | 0.62–1.57 ms |
| 10,000 | 6.02 ms | 5.89–6.67 ms |
| 50,000 | 24.55 ms | 24.42–24.87 ms |

This isolates filtering CPU; it excludes React, DOM, IPC, and network. A fixture-based version is retained in the opt-in UI diagnostics for repeatability; exact field values differ from the standalone baseline above.

### Rust browse baseline

Named tool: optimized `cargo test --release`, `Instant`, actual `browse_view` and full `Library` clone. Libraries contain 10,000 or 50,000 synthetic tracks, 500 artists, 5,000 albums, one genre, no facet selection. One warmup followed by five samples per query. No filesystem or network; excludes Tauri serialization, request scheduling, lock wait, and React response rendering.

| Library | Query/result size | Median | Min–max |
| --- | --- | ---: | ---: |
| 10,000 tracks | Empty / 10,000 results | 37.34 ms | 36.40–37.55 ms |
| 10,000 tracks | `track` / 10,000 results | 37.21 ms | 36.70–37.59 ms |
| 10,000 tracks | `missing-match` / no results | 9.89 ms | 9.81–10.02 ms |
| 50,000 tracks | Empty / 50,000 results | 768.69 ms | 766.24–774.46 ms |
| 50,000 tracks | `track` / 50,000 results | 763.22 ms | 759.22–765.43 ms |
| 50,000 tracks | `missing-match` / no results | 66.84 ms | 66.64–68.43 ms |

Why response size matters: `browse` uses `spawn_blocking`, which keeps synchronous computation off the async runtime, but each request still clones the library, rebuilds facets, sorts selected tracks **before** query filtering, lowercases searchable fields, recomputes counts, and constructs result records. It calls `effective_rating(id)` for every returned track; that calls `get(id)`, a linear track lookup already identified in the Rust audit. Broad results therefore encounter a repeated whole-library lookup. Narrow/no-result searches still pay the upfront clone/facet/sort/filter costs.

The synthetic ~0.77-second browse is **not** a measurement of your library. It demonstrates that the operation started per keystroke can be intrinsically expensive even without disk/network. Supersession checks stop stale display updates, not this work.

## Fix candidates and the tests they need

The immediate targets differ by surface. Blanket debouncing or replacing autocomplete would miss several of these problems.

1. **Importer text entry:** keep the edit buffer in a small input/editor boundary, and propagate a deliberate commit to the review state. Measure before/after native typing with a large batch. Preserve autocomplete suffix selection, blur/Enter persistence, and drafts during background refresh. Do not debounce the displayed text itself.
2. **Main search admission:** make Library browse depend on the actual Library query; Spotify typing should not trigger unfiltered Library work. Coalesce superseded local searches and preserve usable prior results with a pending indicator, while ensuring play/select actions retain the correct result identity. Prove request counts and late-response behavior with held promises, then measure native input-to-paint and query-to-results.
3. **Browse computation:** remove the repeated rating lookup at the owning core/projection boundary, reuse query-independent facets, and avoid sorting/filtering work for superseded requests. The existing Rust audit contains a tested alternative for rating lookup. Re-run the same full-browse benchmark and validate ordering/ratings/facets before claiming a user-visible improvement.
4. **Keyboard prefix navigation:** reconcile global native-control guards with explicit list type-ahead handlers. Preserve a stable candidate list while a facet query is pending, or defer applying the facet while the user builds a prefix. Verify focused row/button behavior, rapid multi-character prefixes, spaces, 1-second timeout, and mouse/keyboard parity. Do not remove native-control protection globally.
5. **Queue filtering:** avoid normalizing every field on every key; first measure whether a reusable normalized search representation and separating filter input from review rendering improve the real window. Keep current token matching across scalar fields. The queue is already virtualized; that alone cannot fix this CPU path.
6. **Single-item Get Info:** no autocomplete rewrite justified by these results. If this is the user's slowest surface, reproduce on the native field with their actual suggestion set and editing pattern. Bulk editing, IME/composition, native selection rendering, and mount-time suggestion loading remain distinct unmeasured cases.

Suggestions are only fetched on mount, but their backend construction is not free: `metadata_values` clones the library and scans/sorts distinct artists/albums/genres; `genre_values` calls the same collector then discards artists and albums. That can delay initial availability of suggestions, but it does not explain every keystroke after suggestions have loaded.

## Evidence status and reproduction

- **Confirmed behavior:** per-key browse requests, extra unfiltered browses in Spotify scope, stale-track hiding, keyboard admission/refinement problems, and no IPC during autocomplete typing.
- **Measured synthetic baselines:** React render work, local filtering CPU, and optimized browse projection cost above.
- **Optimization decision: UNVERIFIED.** No production change or comparable native before/after trace exists. There is no authenticated performance-recommendation evidence bundle; do not interpret these tests as measured production gains. Next step is a native trace and a narrowly scoped change against the demonstrated path.
- Baseline/tested lineage: `bdfc933` plus diagnostic-only additions; production source unchanged. Existing tests and accessibility behavior checks remain in place.
- Validation: 95 frontend helper/gateway tests and 25 mounted interaction tests passed with both audit suites enabled. The optimized Rust browse benchmark, frontend build/lint, Rust formatting, strict Clippy, and diff whitespace checks passed.

From `apps/desktop`, run the mounted diagnostics (six new cases):

```sh
rtk proxy env RETUNE_TYPEAHEAD_AUDIT=1 npx vitest run test/interactions.test.tsx -t 'type-ahead audit' --disableConsoleIntercept
```

From repository root, run the optimized backend diagnostic:

```sh
rtk proxy cargo test -p retune-desktop --lib --release audit_typeahead_browse_costs -- --ignored --nocapture
```

Sources: [shared autocomplete and edit dialogs](../apps/desktop/src/dialogViews.tsx), [main input/effects/keyboard routing](../apps/desktop/src/App.tsx), [shortcut and request-key helpers](../apps/desktop/src/ui.ts), [query reducer](../apps/desktop/src/appState.ts), [track/facet controls](../apps/desktop/src/libraryViews.tsx), [importer](../apps/desktop/src/LastFmImporter.tsx), [queue filter](../apps/desktop/src/lastfmImportState.ts), [browse/metadata commands and benchmark](../apps/desktop/src-tauri/src/library_commands.rs), [library snapshots](../apps/desktop/src-tauri/src/library_state.rs), [core browse](../crates/retune-core/src/browse.rs), [rating lookup](../crates/retune-core/src/model.rs), [UI diagnostics](../apps/desktop/test/interactions.test.tsx).
