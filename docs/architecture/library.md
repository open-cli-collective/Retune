# Library domain

`retune-core` is Retune's deterministic domain layer. It contains no filesystem,
network, async, Tauri, or UI code. Callers provide data and persist the result.

## Records and identity

`TrackId` is a stable local integer. A track's provider URI is its deduplication
identity; local imports use canonical `file://` URIs. A record contains its
source, category/artist/album/name overlay, duration, track/disc numbers, release
date, rating, play count, timestamps, kind, bitrate, and original provider category.
Tracks are enabled for sequential playback unless the overlay explicitly disables them.

Album identity is deliberately `(source, artist text, album text)`. Editing an
artist or album therefore re-parents a track, and normalizing two editions to the
same text merges their Retune album group.

## Overlay rules

- Track rating overrides album rating; absent track rating inherits the album.
- The first category divergence records the provider category. The override
  marker is shown only while current and original categories differ.
- Provider upsert refreshes track/disc, release date, kind, bitrate, and provider category.
  It preserves overlay name/artist/album/rating and play history. `added_at` is
  backfilled when missing and takes the earliest credible discovered value; a
  later provider timestamp never moves it forward.
- Adding or merging deduplicates by URI. Existing overlay values win.
- Provider refresh preserves each track's playback-enabled overlay value.
- Restoring replaces the library after validating the imported envelope.

Overlay edits never mutate source-file tags or Spotify metadata.

## Last.fm historical import

Last.fm import is an application-shell boundary in
`apps/desktop/src-tauri/src/lastfm_import.rs`; `retune-core` remains a pure,
deterministic mutation target. History is an absolute baseline: resolved source
counts use one reusable account-bound Sum, highest-played spelling Overwrite, or
Zero default across unlocked Spotify targets. Acceptance freezes the strategy
used by each selected target, then local play count takes the maximum of current and historical
values. `last_played_at` takes the latest relevant scrobble and `added_at` takes
the earliest; Zero never erases known Retune plays. Blank genre/rating options
are no-ops. Whole-album rating is stored as an album rating, while
selected-track mode stores explicit track ratings only.

Each page has two independent intents: import Spotify content and include
historical play counts. Both default on, at least one must remain on, and
whole-album is a separate page-level content mode. It defaults on only when one
Spotify album maps every included source track one-to-one with no extra tracks;
the user's persisted page choice then wins. Content-only
acceptance saves membership and applies source `added_at` without changing
plays or `last_played_at`; counts-only performs no Spotify write and updates
only already-materialized matched Retune tracks.

The importer’s “Show Spotify search terms” preference is session-level and is
restored on resume. Fuzzy disclosures are bounded to the visible persisted
`ImportBatch`; the target-wide count decision still includes completed source
rows from other batches. Last.fm rows with an empty source album are collection
rows, displayed as `Singles`, and are matched one track at a time. Ratification
prefers accepted mappings and manual choices, then uniquely exact normalized
title/primary-artist candidates already in the Retune library or exact saved
Spotify track/album membership, then uniquely exact candidates without
ownership. Same-artist near matches are suggestions only; wrong artists from
ordinary search remain unresolved. Within the user's explicitly selected album
set, a unique exact-title track auto-selects even when Spotify credits it to a
different artist; multiple exact-title targets remain ambiguous. Catalog
comparison is diacritic-insensitive. Near-title comparison
ignores parenthetical annotations, accepts bidirectional token containment, and uses token overlap only to order otherwise
equal candidates within the visible batch. It also tolerates one inserted or
missing character or one adjacent transposition in a single title token of at
least five characters. Generated track searches apply the
same parenthetical simplification rather than issuing a second Spotify request.
Exact or near-title tracks found on multiple selected albums remain unresolved
until the user chooses an album-labelled candidate; the UI recommends one only
when its existing matched/unique album coverage strictly outranks the
alternatives. Collection review persists a V2,
batch-keyed cache of preview candidates and ordered selected album URIs; its
coverage is derived over the selected union, and cached add/remove/revisit
operations rerank without Spotify requests or membership writes. Search makes
one album request; the first Preview or direct Add fetches one album, while
cached operations make no request. Legacy album-shaped search terms refetch
only the incompatible rows. A non-empty source album literally
named `Singles` remains release-shaped.

Release-shaped batches compare source and Spotify album and track titles with
the same normalized contained-token rules used by collection matching. Retune
automatically selects a release only when it is the sole title-compatible
candidate that either maps every source row one-to-one or covers at least 80% of
both the source rows and the Spotify album's distinct tracks; clean Spotify
supersets are therefore selected despite extra tracks. Artist credit is
supporting metadata rather than a veto because soundtracks and compilations are
commonly credited differently across catalogs. Generated album searches elide
parenthetical annotations while retaining Spotify's album and artist filters.
Unmatched source rows remain available for individual review. Cached
unselected album results are reclassified locally on load and do not trigger a
new Spotify request. Manual album searches apply the same automatic selector
while preserving prior explicit track or album choices.

Track-match review mutations keep their existing account guards, atomic
persistence, and import-change event, while `lastfm_import_select_match` and
`lastfm_import_change_track` also return the affected `ImportPageView` for
direct installation on the visible page. Choosing a cached candidate does not
make a Spotify request. `lastfm_import_select_matches` applies a non-empty set
of candidates from one review batch in one atomic session write and returns the
same page projection; the collection UI uses it to ratify all currently valid
same-artist suggestions without issuing Spotify requests.

The visible review-page projection stably partitions actionable selected rows
whose IDs are in `requiredImportMatchIds` (selected rows without a track target)
ahead of the remaining rows, preserving source order inside both groups. A
successful mapping therefore moves that row below any remaining work without
changing the persisted source order. For collection rows, a same-artist
`same-songs` track is never ratified automatically: the UI shows it through
SUGGESTED / Use This Track only when exactly one distinct target URI remains
after deduplication and the target is already in the library or belongs to a
selected album-match union. Wrong-artist, low-confidence, and multiple-target
near matches use the same album-labelled chooser; low-confidence matches remain
unresolved.

The source importer is V2. It records a fixed profile-bound `historyTo`, probes
Last.fm metadata once, downloads pages at the documented 200-row limit; Last.fm's
page total is oldest, so Retune moves toward page 1, and stores parsed raw pages under a snapshot-specific
machine cache. The metadata probe remains sequential; after the total is known,
bounded windows of four source requests are launched together and
processed/checkpointed in descending page order. Rows at or after `historyTo` are rejected before caching or
counting; the exact Last.fm username is recorded in the manifest and each page.
The manifest is authoritative: an orphan page file is harmless and may be
overwritten, while an acknowledged missing, corrupt, oversized, or
metadata-mismatched page quarantines the whole snapshot and restarts V2. The
manifest and session cursor remain a contiguous descending suffix; a failed
window retains its already-checkpointed prefix, discards lower in-memory
results, and resumes from the failed page. A process exit can therefore
refetch only the unfinished part of one window. Concurrent workers return
structured outcomes without mutating importer retry state; the ordered
coordinator persists one attempt for a retryable failure, waits at the capped
backoff, and re-enters from the failed cursor without advancing it. No aggregation happens until every page
is acknowledged; then raw-page reads, sorting, and aggregation run off the
async runtime before review is entered atomically (or Done when no rows remain)
and the cache is best-effort deleted. A saved Downloading or Aggregating session
is claimed and resumed once by the Tauri shell after Last.fm hydration using its
stored username and cutoff; Downloading remains Last.fm-only, while Aggregating
also verifies the live Last.fm username before claiming the runner and suspends
with redacted state on mismatch. The owner is revalidated and held through the
aggregation transition and emitted state, so a connected-account change cannot
expose the completed snapshot. React only observes that work. An empty state
does not create one.

### Last.fm incremental reconciliation

The first exact sync establishes `syncedThrough=now`; it does not backfill
older post-enable history. Launch, reconnect, and the explicit “Sync Last.fm
plays” action each process one fixed half-open window. The query is padded for
Last.fm’s exclusive bounds and the exact window is filtered locally. Incremental
download reuses the historical parser, 200-row pages, four-page downloader,
manifest/cache, retry protections, and oldest-to-newest checkpoints. It never
calls Spotify and does not aggregate until the range is complete.

Retune-origin scrobbles are represented by accepted local receipts and are
matched as a multiset, so they are not imported again. Every other accepted
mapped event increments its materialized Retune track additively and advances
`last_played_at` to the newest external timestamp; the historical play-count
threshold does not apply. An explicit track mapping wins over an album mapping.
Unknown or unavailable targets remain in the stable importer backlog, which can
accept later windows without blocking their download. Explicit accepted matches
and permanent track, album, and artist ignore rules are reusable and sweep
applicable backlog occurrences; Skip is temporary. Accept & Next archives a
reviewed batch from the queue, records mappings only for selected rows, and leaves
future occurrences of unselected rows eligible for review. No target is silently added
to the library: explicit reviewed content choices use the existing Spotify save
operations.

The Tauri shell’s pure reconciliation function receives remote events, accepted
receipts, mappings/ignore rules, and available library URIs. It returns additive
increments, latest timestamps, unresolved events, and consumed receipts without
filesystem, HTTP, async, Tauri, or Spotify concerns. A before/after application
journal is persisted before the atomic library write and records checkpoint,
backlog, and receipt effects. Recovery applies the before state, finalizes the
after state, or exposes a typed conflict; it never guesses.

Review batches are stable 1-based `ImportBatch` pages capped at 100 source rows.
Normal albums under the cap remain one batch; larger albums and singles split
deterministically, and command arguments must identify the requested batch and
its artist/album identity. Row actions also require a source ID; album-level
actions can operate without one. Queue summaries cross the IPC boundary as
bounded cursor/limit pages with `sourceCount`, not source IDs. Queued and
running apply jobs are omitted from the active projection; failed jobs reappear
with their frozen choices and an explicit retry action. A failed Accept All job
parks its cursor until that frozen job succeeds, then resumes from its saved
bulk index. Accept All persists one
compact cursor and advances one durable apply job at a time; review choices are
locked while that cursor is active, including after restart.

Spotify matching is lazy. Opening a visible batch uses the shared client and
request gate, serializes duplicate requests, binds the Spotify account on the
first match, and caches the result. Once that page is visible, React requests
exactly the next batch in the active sort order as a one-item lookahead; an
overlapping foreground open joins the same importer lock and cached result.
Correctly cached collection batches reopen
without an API call; only legacy empty-album rows with album-shaped cached
search terms refetch. Whole-album import stays disabled for a collection until
one coherent release is explicitly chosen. Accept All prepares all remaining
batches sequentially before
showing global unique album/track URI counts and awaiting confirmation. Excluded rows remain
source-history decisions and can be undone before acceptance; they never remove
a track inherently materialized by a saved whole album.

Accepting one batch first persists a frozen, account/session-bound apply plan in
`lastfm-sync.json`; the command waits only for that atomic enqueue. A serial Rust
worker then saves upstream membership, materializes missing tracks, applies
idempotent history/metadata, persists reusable mappings, and finally commits the
review decision before removing the job. A process exit or failure leaves the
job at its last stage for safe replay; account changes suspend it without
applying to another profile. The UI advances from the authoritative queue after
enqueue and completion events never replace a newer selection.

Spotify saved-track and saved-album memberships are not core-library state.
The shell keeps those account-scoped memberships separately while materializing
their Spotify tracks here for playback, ratings, and play counts. A materialized
track may therefore be individually saved, referenced by one or more saved
albums, both, or neither.

## Browse projection

The three-column browser is a pure projection over `Library`. `Selection`
contains source/category/artist/album filters; `Facets` and visible tracks are
derived from it. Choosing a broader facet clears invalid narrower selections.
When a resolved projection proves a preserved value stale, the UI falls back
hierarchically by clearing category, artist, and album for a missing category,
or artist and album for a missing artist or album.
Alternate views should consume the same library rather than introduce another
canonical model.

The UI keeps the last resolved projection visible while the same selection is
refreshed. A source, facet, search, or scope change invalidates it until that
new projection resolves, so playback cannot consume rows from the prior view.
Double-clicking a facet row waits for that exact projection, then starts its
first visible track with the full projection as the new queue.

## Track sorting

An explicit column sort uses that column as its primary key, followed by disc,
track number, artist, album, and category as stable tie-breakers (omitting the
primary key when it is already in that list). Sorting by track number treats
disc as the first key so multi-disc albums remain in playback order. A missing
disc number means Disc 1; other missing values remain last. The selected
direction applies to every key. Text comparison is locale-aware and
case-insensitive. With no explicit sort, the browse projection's
album/disc/track order is preserved.

## Serialization

Core exports use a versioned JSON envelope and optionally gzip at the application
boundary. Import rejects duplicate IDs and URIs and recomputes the next local ID.
Application backup adds settings and playlist cache data around the core
envelope; see [Persistence](persistence.md).

## Change guidance

Keep provider translation out of this crate. New behavior belongs here only when
it can be expressed and tested as a deterministic transformation of library
state.
