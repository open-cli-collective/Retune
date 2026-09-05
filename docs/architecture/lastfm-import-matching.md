# Last.fm import matching

This document is the durable reference for how Retune turns Last.fm source rows
into Spotify album and track targets during historical import and incremental
reconciliation. It describes decision policy and ownership; UI layout belongs
in the React importer and Spotify transport policy belongs in the
[Spotify domain](spotify.md).

## Ownership and invariants

Matching is exposed by the `apps/desktop/src-tauri/src/lastfm_import` facade;
persisted and boundary types live in `model`, filesystem formats in `store`,
and the serialized runtime owner in `service`. `source` and `clustering` prepare
source rows and batches; `matching`, `collection`, and `review` own deterministic
candidate and page projection; `apply`, `incremental`, and `reconciliation` own
durable execution; `coordinator` owns shared workflows; and `commands` is the
Tauri adapter. The Rust service
receives compact Last.fm `SourceRow` values, searches through the shared Spotify
client, persists candidates and choices in the import session, and projects an
`ImportPageView` to React. `retune-core` receives only resolved library mutations
and remains free of network, async, filesystem, Tauri, and matching concerns.

`lastfm_import/commands.rs` is the sole Tauri and `AppState` shell. Each command
maps IPC values, resolves one importer `UseCases` bundle, makes one application
call, and adapts its result to typed events or output. The Tauri-free
`application` module owns review, matching, incremental reconciliation, and
durable apply orchestration through concrete Last.fm, library,
Spotify-membership, settings, cooldown, provider, and connection owners plus
narrow worker/event callbacks. Lower stages never receive `AppHandle` or
`AppState`.

The Last.fm connector owns recent-track pagination policy and accepted-scrobble
receipt metadata. The importer depends on those connector models; the connector
does not depend on importer state. The application shell coordinates incremental
scheduling when authorization finishes or an account disconnects. Disconnect
retains the username-scoped import session and incremental snapshot cache; a
different authenticated username replaces that scoped state when sync resumes.

Snapshot and incremental cache IDs are one non-empty normal path component and,
when their identity inputs are available, must equal the recomputed ID. Cache
reads, writes, renames, cleanup, invalidation, and quarantine independently
reject absolute, parent, root, empty, and symlink targets, so persisted state
cannot escape `lastfm-import-cache`. Invalid state is quarantined without
touching the referenced cache. `Service` owns the single runner claim and
serialized session transition; React observes progress and never starts a
parallel importer runner. Import and incremental retry waits remain owned by
that runner and select between their timer and the Last.fm service lifecycle
signal. Disconnect, reconnect, invalid-session replacement, and application
shutdown advance the lifecycle generation, wake sleepers, and prevent the old
generation from issuing another request. Closing the importer window does not
cancel checkpointed work.

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

Last.fm state and queue projections read the shared cooldown store's authoritative
effective deadline rather than retaining endpoint-specific retry timestamps. A
rate-limited or quota-failed queue item projects that deadline, and clears its
stale retry time when the persisted cooldown expires or is emptied; ordinary
apply failures retain their own retry metadata. Searches still use the shared
Spotify client: only a network-backed search can clear the global Development
Mode quota, while a persistent catalog hit is side-effect free.

## Source shapes

Aggregation groups spelling variants into stable source rows while retaining
their raw names, play counts, and timestamps. Review batching is a deterministic
two-pass projection. Pass one keeps the historical exact `(artist, album)`
groups. Pass two merges only groups with pairwise support: compatible artist
credits need either a fuzzy album-title match or at least two matching tracks
covering 80% of the smaller group; different credits need both fuzzy album-title
support and at least three matching tracks with the same coverage. Distinct
numbered series entries never merge, including context-marked Roman numerals.
An empty-album row joins a named cluster only when its artist and track identify
exactly one cluster; otherwise it remains in `Singles`.

The highest-play exact group supplies the cluster's representative artist and
album label. A cluster containing more than one exact group is collection-shaped
and retains all source album labels for disclosure and album-level review
actions. Persisted legacy batches with page-scoped choices or queued apply work
keep their page identity; untouched legacy batches are rebuilt without losing
row-scoped matches or decisions. Review then has two shapes:

- A non-empty Last.fm album is **release-shaped**. Retune searches for an album,
  compares the source track set with each Spotify release, and may select one
  release for the batch. Review retains `Change Album…` and also offers
  `Add Album…`; choosing it switches that batch to collection matching using
  the cached release candidate, without another Spotify request. The source
  album label remains visible, but the batch thereafter uses the collection
  album controls. Each selected album independently chooses full-album library
  membership; that choice does not affect the album union used for matching.
- An empty Last.fm album is **collection-shaped** and displayed as `Singles`.
  Retune matches each source row against individual tracks. The user may build a
  set of Spotify albums whose track union constrains and improves those matches.
  A literal album actually named `Singles` remains release-shaped.

Persisted batches preserve complete source clusters without a row-count cap.
Queue projections report imported and remaining play totals separately so a
cluster containing completed rows does not present its full history as new
work. Queue-filter keystrokes remain local to the queue control and coalesce
before updating its projection, so they do not reconcile the visible review
draft on every character. The genre field likewise owns its live draft and
flushes it on blur or Enter and before any whole-options write or Apply; page
refreshes still merge authoritative rows with the current batch draft.
`genre_values` projects only distinct genres instead of constructing unused
artist and album suggestions. The visible batch is the unit of lazy Spotify
work and review. Review exclude/undo actions
may address one or more source IDs, but every ID must belong to the requested
batch and remain reviewable; empty, cross-batch, and completed-row requests are
rejected before any decision changes. A bulk action persists the session and
reusable mappings once. Track exclusions do not rebuild the incremental backlog
on the review click path; the normal incremental-sync entrypoint applies those
durable mappings before fetching more plays. Album and artist cascades still
sweep applicable backlog immediately.

The user may combine any two or more queue batches into one custom
collection-shaped batch without a row-count cap. Bulk row actions likewise
accept the whole custom batch. A mixed-artist batch is labeled
`Various Artists`. Combining preserves row decisions, matches,
compatible batch options, and any existing collection album choices;
whole-release mode becomes the per-album choice used by collections. Custom
batch membership is persisted and retained when incremental sync rebuilds the
remaining automatic batches. Skip affects exactly the custom batch's source
rows. Album- and artist-wide ignore are unavailable because the arbitrary
grouping does not represent one source entity.

## Search and cache boundary

Release matching issues one bounded Spotify album search using generated
`album:` and `artist:` fields, then obtains complete candidate track lists for
local comparison. Generated album and track searches elide parenthetical
annotations. Search results are capped at ten candidates.

Collection-shaped batches with no representative album do not search Spotify
automatically. A named collection cluster lazily searches its representative
artist and album once, using only rows from that exact source group to evaluate
the release rather than requiring one album to cover the entire merged cluster.
The existing bounded release gate hydrates only the strongest title tier. A sole
supported release seeds the selected album union and reranks every row in the
collection locally; absent or ambiguous results remain manual. The attempt and
hydrated candidates are persisted even when no release is selected, so revisit
does not search again. No collection path issues automatic per-track searches.

The user can still add likely albums or explicitly change a track; the selected
albums' cached track union then drives local matching. Explicit collection-album
search accepts ordinary user text as well as Spotify field syntax. Search
returns album summaries; the first Preview or Add obtains the complete album,
after which preview, add, remove, revisit, and reranking are local. Opening a
cached collection page never resolves `/me`. Its candidates and mappings remain
scoped to the session's Spotify account ID, so an unknown or disconnected
membership snapshot still permits local reranking; an explicitly different
account suspends the session.

Release search hydrates track lists only for summaries in the strongest
album-title tier. Known advertised track counts are ranked by proximity to the
source-row count, but a smaller release remains eligible because multiple source
rows may collapse onto one Spotify track. At most three summaries are hydrated,
in deterministic title/count/artist/provider order. Exact artist credits precede
loose compatible credits, which precede unrelated credits. An explicit album
query may hydrate a result outside the automatic summary gate.

Search and album/track observations also enrich the shared materialized Spotify
catalog. Exact search result identities persist there without expiry, scoped by
immutable Spotify account ID, so the same generated query can resolve locally
across import sessions, app restarts, and disconnect/reconnect cycles. The import
session separately persists the hydrated candidates and explicit choices needed
to resume the workflow. The catalog is machine-local and discardable; accepted
Last.fm mappings are portable profile data.

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
   Collection matching applies this final fallback across the selected album
   union and leaves equally strong distinct tracks ambiguous.

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

Supported candidates are ranked first by the number of source rows they match,
then by the fewest Spotify tracks. Retune automatically selects the sole
candidate with the strongest coverage and tightest scope. Thus an edition that
matches every source row wins over one that misses a row, and an 11-track
release wins over a 13-track release when both cover the same source rows.
Distinct releases tied on both measures still require a choice. Cached named
collection candidates are reevaluated locally under the same rule; explicitly
removing the selected album disables automatic reselection for that batch.
Release dates do not break remaining ties because Spotify describes that value
as the album's first release date, not a reliable edition or remaster date.

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
One explicitly chosen track candidate may be applied to multiple source rows in
the same review batch; each row keeps its own source identity and contributes to
the normal target-wide count merge.

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

Rows that skipped automatic Spotify search still receive local match state from
the selected album union, so a unique exact album-track match does not require a
manual picker choice.

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
are derived from the selected union, not from all search results. A separate
pressed `Add to library` state selects any subset of that union for full-album
membership and defaults on for albums already in the library.

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

Accepting a batch freezes an account- and session-bound apply plan. Release
whole-album mode saves one album URI. A collection plan saves every pressed
album in full plus distinct resolved track URIs not covered by those albums;
unpressed albums still participate in matching. Counts-only mode performs no
Spotify membership write. The asynchronous
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
