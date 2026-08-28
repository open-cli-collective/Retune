# Last.fm import matching

This document is the durable reference for how Retune turns Last.fm source rows
into Spotify album and track targets during historical import and incremental
reconciliation. It describes decision policy and ownership; UI layout belongs
in the React importer and Spotify transport policy belongs in the
[Spotify domain](spotify.md).

## Ownership and invariants

Matching is owned by `apps/desktop/src-tauri/src/lastfm_import.rs`. The Rust
service receives compact Last.fm `SourceRow` values, searches through the shared
Spotify client, persists candidates and choices in the import session, and
projects an `ImportPageView` to React. `retune-core` receives only resolved
library mutations and remains free of network, async, filesystem, Tauri, and
matching concerns.

The matching boundary preserves these invariants:

- Spotify requests use the shared client, request gate, cooldowns, and
  install-local materialized catalog.
- Opening an already-matched batch, revisiting cached candidates, changing a
  cached choice, or adding/removing a cached collection album makes no Spotify
  request.
- Accepted mappings and explicit manual choices outrank new heuristics.
- A heuristic may select only one strongest target. Equally strong distinct
  targets remain unresolved for the user.
- More than one Last.fm source row may map to the same Spotify track. Count
  merging is intentional and independent of candidate ambiguity.
- Matching never changes Spotify membership. Only explicit import acceptance
  may save an album or tracks.

## Source shapes

Aggregation groups spelling variants into stable source rows while retaining
their raw names, play counts, and timestamps. Review then has two shapes:

- A non-empty Last.fm album is **release-shaped**. Retune searches for an album,
  compares the source track set with each Spotify release, and may select one
  release for the batch. Review retains `Change Album…` and also offers
  `Add Album…`; choosing it switches that batch to collection matching using
  the cached release candidate, without another Spotify request. The source
  album label remains visible, but the batch thereafter uses the collection
  album controls and cannot use whole-album mode.
- An empty Last.fm album is **collection-shaped** and displayed as `Singles`.
  Retune matches each source row against individual tracks. The user may build a
  set of Spotify albums whose track union constrains and improves those matches.
  A literal album actually named `Singles` remains release-shaped.

Persisted batches are capped at 100 source rows. A large artist/album group can
span batches without changing its artist-level ignore behavior. The visible
batch is the unit of lazy Spotify work and review. Review exclude/undo actions
may address one or more source IDs, but every ID must belong to the requested
batch and remain reviewable; empty, cross-batch, and completed-row requests are
rejected before any decision changes. A bulk action persists the session and
reusable mappings once, then performs one backlog sweep.

## Search and cache boundary

Release matching issues one bounded Spotify album search using generated
`album:` and `artist:` fields, then obtains complete candidate track lists for
local comparison. Generated album and track searches elide parenthetical
annotations. Search results are capped at ten candidates.

Collection rows use bounded track searches. Explicit collection-album search
accepts ordinary user text as well as Spotify field syntax. Search returns album
summaries; the first Preview or Add obtains the complete album, after which
preview, add, remove, revisit, and reranking are local.

Search and album/track observations also enrich the shared materialized Spotify
catalog. The import session separately persists the candidates and explicit
choices needed to resume the workflow. The catalog is machine-local and
discardable; accepted Last.fm mappings are portable profile data.

After a visible batch resolves, React requests one lookahead batch in the active
sort order. Retune does not match the entire queue eagerly. Accept All is the
explicit bulk exception and prepares remaining batches sequentially before its
counted confirmation.

## Title normalization and track selection

Catalog comparison is case- and diacritic-insensitive. Punctuation is ignored
for compact equality. Near-title comparison removes parenthetical annotations,
tokenizes alphanumeric words, and then considers bidirectional token
containment. It also tolerates one inserted or missing character or one adjacent
transposition in a token of at least five characters.

When mapping a source title within one selected release, Retune evaluates the
most specific evidence first:

1. compact normalized title equality;
2. normalized token-sequence equality after parenthetical removal;
3. one unique target whose compact title begins with the source title;
4. one unique bidirectional contained-token or tolerated-typo target.
5. for release-shaped rows only, retry after removing a dash-delimited trailing
   source artist or source album that exactly matches the batch metadata;
6. one unique best target sharing at least two tokens longer than three
   characters and at least half the meaningful tokens of the shorter title.

This ordering prevents a broad match from hiding a more specific edition. For
example, `Raise Your Banner (feat. Anders Fridén) [Single Edit]` selects
`Raise Your Banner - Single Edit` before the base `Raise Your Banner` track.
If the strongest stage still produces multiple distinct targets, the row stays
unresolved. Spotify qualifiers remain intact: repeated titles such as `Main
Theme (From "Jurassic Park")` and `Main Theme (From "Schindler's List")` tie
instead of collapsing to `Main Theme`.

Spotify album tracks include duration and Retune caches it for collection album
previews. Last.fm recent-play history does not supply source duration, so
duration is not a historical-import match signal unless a future cached source
provides it; Retune does not issue per-track metadata calls solely to obtain it.

Artist credit supports ranking but does not veto a release-shaped match.
Soundtracks, classical releases, featured performers, and compilations are
frequently credited differently across Last.fm and Spotify.

## Release matching

Each album candidate is classified from source-to-track coverage:

- **Best match**: every source row maps one-to-one and the source and Spotify
  track counts agree.
- **Superset**: every source row maps and the Spotify release has additional
  tracks.
- **Same songs**: at least half of the source rows map.
- **Unclassified**: insufficient track-set evidence.

Automatic selection considers only title-compatible classified candidates with
non-empty track lists. A candidate is supported when either every source row
maps one-to-one, or at least 80% of source rows map and those distinct targets
cover at least 80% of the Spotify release.

Supported candidates are ranked `Best match`, `Same songs`, `Superset`, then
unclassified. Retune automatically selects the sole candidate at the strongest
available rank. Weaker supported releases do not create false ambiguity. Thus a
15-track `Best match` for `Babel (Deluxe Edition)` wins over a 12-track standard
edition classified as `Same songs`; two distinct 15-track best matches still
require a choice.

Selecting a release remaps every compatible source row against that release.
Rows not supported by its track set remain visible for individual review. Cached
unselected candidates are reclassified on load, so improvements to deterministic
matching apply without another Spotify request.

Manual album and track searches also accept canonical Spotify URIs,
`spotify://` links, and `https://open.spotify.com` share links. A pasted link
resolves that exact entity through the materialized catalog/shared client rather
than running text search. Explicitly choosing an exact album link may keep
unmatched source rows for individual review; it does not manufacture mappings.
The track picker first offers tracks already cached for the selected release or
selected collection album set, then offers global Spotify search as a fallback.
Choosing a cached track makes no Spotify request; Rust verifies that its URI
belongs to a currently selected album before persisting the mapping.

## Collection matching

The selected collection albums form a deduplicated union of Spotify track URIs.
Accepted mappings and manual choices win first. Remaining rows prefer exact
normalized titles over near titles. Within the same title tier, existing Retune
or exact saved-Spotify membership and matching artist text improve rank but do
not override a stronger title.

A unique exact candidate can be selected automatically. A unique same-artist
near candidate belonging to the selected album union or existing library is a
visible suggestion, not an automatic decision. Multiple strongest target URIs
remain ambiguous and are shown with album labels. The UI may recommend an album
only when its projected matched and unique coverage strictly outranks the other
choices.

Switching a release-shaped batch into collection matching is an explicit,
idempotent persisted mutation. It seeds the selected release into the cached
album set, clears only the release-level selected URI and persisted
whole-album option, and reranks through the same collection matcher. Accepted
mappings and explicit track choices are retained. The actual non-empty source
album is used for every subsequent collection command, so adding or removing
albums and revisiting the page remain scoped to that batch. Empty-album
collection batches and untouched release-shaped batches keep their existing
behavior.

Adding or removing an album reranks unresolved automatic rows locally while
preserving accepted mappings and explicit manual choices. Coverage summaries
are derived from the selected union, not from all search results.

The review UI groups already-selected library track matches by their Spotify
album as automatic contributors. These groups are informational: only albums
the user explicitly adds belong to the selected album union and can be removed
from the match set.

## Confidence, required work, and count merges

`Exact`, `Likely`, and `Low` describe evidence for the projected target; they do
not themselves change membership or play counts. A selected actionable row is
`ACTION REQUIRED` only when it has no supported Spotify track URI. Required rows
are stably partitioned above resolved rows without changing persisted source
order.

Several source rows may intentionally resolve to one Spotify URI—for example,
plain, featured-artist, and tag variants of one recording. The target-wide
Sum/Use highest/Zero mode determines the historical count. The chosen default is
reused for unlocked targets across import sessions; accepting a target freezes
its mode. Completed source rows from other batches participate when they map to
the same target.

In the `ImportPageView` projection, `fuzzyGroups` remains scoped to the
current batch for source disclosure, while `resolvedCounts` is the
authoritative target-wide result including eligible completed rows, resolved
with the selected count mode.

## Acceptance and reusable mappings

Accepting a batch freezes an account- and session-bound apply plan. Whole-album
mode saves one album URI; selected-track mode saves the distinct resolved track
URIs. Counts-only mode performs no Spotify membership write. The asynchronous
Rust worker then applies remote membership, local materialization and history,
metadata, reusable mappings, and review decisions in checkpointed order.

Accepted track mappings and permanent album/artist ignore decisions are reused
by incremental Last.fm reconciliation so later external-device scrobbles can be
applied without repeating review. Skip is temporary. Unselected or unresolved
rows do not silently create a mapping or add content.

## Change checklist

When matching behavior changes:

1. Change the deterministic rule at its Rust owning boundary.
2. Add a focused regression using the smallest real-world shape that proves the
   decision and its ambiguity boundary.
3. Verify cached revisits do not add Spotify requests.
4. Update this document when precedence, thresholds, persistence, or API usage
   changes.
5. Exercise the affected importer journey in the installed desktop app.
