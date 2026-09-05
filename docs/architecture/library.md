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
- Shell callers read track records immutably. Core methods record plays, merge
  history with saturating counts and monotonic timestamps, and fill only missing
  technical metadata without exposing mutable identity fields.
- Projections compute effective ratings from their already-resolved canonical
  track record. Batch Get Info and technical-metadata fills validate every ID
  before mutating, then use one transient ID index for the batch.
- Restoring replaces the library after validating the imported envelope.

Overlay edits never mutate source-file tags or Spotify metadata.

The desktop shell composes the live `Library`, its filesystem store, write
gate, and long-running transaction exclusion as one concrete `LibraryState`.
Ordinary changes clone the current library, save the candidate, and only then
swap live memory. An unchanged candidate completes under the same mutation and
restore gates without writing or replacing the live value. Spotify membership receives a borrowed `LibraryOwner`
capability for that same boundary; it does not acquire unrelated application
state. Local import and multi-component restore use narrow owner-held exclusive
seams so their established transaction and lock ordering remains explicit.

Local import scans supported audio recursively without following directory
symlinks. Inaccessible or disappearing paths are reported individually while
supported files from accessible siblings continue through import.

## Last.fm historical import

Last.fm import is an application-shell boundary behind the `Service` facade in
`apps/desktop/src-tauri/src/lastfm_import.rs`. The facade re-exports the narrow
runtime surface; private `service`, `model`, and `store` modules own state and
persistence; `source` and `clustering` own ingestion and review grouping;
`matching`, `collection`, and `review` own candidate and page projection;
`apply`, `incremental`, and `reconciliation` own durable execution; and
`coordinator` plus `commands` own application orchestration and Tauri adaptation.
The service holds the session lock while it clones a sync snapshot, then pure
review functions build queue and page command DTOs from the borrowed session
and owned snapshot. Those functions perform no filesystem or provider access.
Pure option, count-strategy, review-action, and match-selection transformations
return candidate records to the service for publication and persistence.
Persisted session, mapping, apply-job, and journal records remain owned by their
existing stores.
Read-only cooldown projections hydrate the shared cooldown store once, filter
expired entries in memory, and never rewrite the file. Mutating cooldown
operations normalize legacy quota keys before applying their update and retain
the existing atomic persistence boundary.
`retune-core` remains
a pure, deterministic mutation target. History is an absolute baseline: resolved source
counts use one reusable account-bound Sum, highest-played spelling Overwrite, or
Zero default across unlocked Spotify targets. Acceptance freezes the strategy
used by each selected target, then local play count takes the maximum of current and historical
values. `last_played_at` takes the latest relevant scrobble and `added_at` takes
the earliest; Zero never erases known Retune plays. Blank genre/rating options
are no-ops. Whole-album rating is stored as an album rating, while
selected-track mode stores explicit track ratings only.

Each page has two independent intents: import Spotify content and include
historical play counts. Both default on, at least one must remain on, and
whole-album is a separate page-level content mode for release-shaped batches. It
defaults on only when one Spotify album maps every included source track
one-to-one with no extra tracks; the user's persisted page choice then wins.
Collection batches instead keep matching and membership independent: every
album in the selected match set has its own `Add to library` toggle, and an
album already in the library defaults on. Pressed albums are saved in full;
resolved tracks not covered by a pressed album are saved individually. Content-only
acceptance saves membership and applies source `added_at` without changing
plays or `last_played_at`; counts-only performs no Spotify write and updates
only already-materialized matched Retune tracks.
Enabling whole-album mode repairs an empty source-row selection by selecting all
actionable, non-excluded rows; an existing partial selection remains unchanged.

The importer’s “Show Spotify search terms” preference is session-level and is
restored on resume. Fuzzy disclosures are bounded to the visible persisted
`ImportBatch`; the target-wide count decision still includes completed source
rows from other batches. Last.fm rows with an empty source album are collection
rows, displayed as `Singles`, and are matched one track at a time. Ratification
prefers accepted mappings and manual choices, then the strongest unique local
title match from Spotify's already-scoped results. Exact normalized titles win
over near titles; saved Retune/Spotify membership and matching artist text rank
candidates within the same title tier but do not veto a match. The same rule
applies to the user's selected album set, and multiple candidates in the
strongest title tier remain ambiguous. Catalog comparison is
diacritic-insensitive. Near-title comparison ignores parenthetical annotations,
accepts bidirectional token containment, and uses token overlap only to order
otherwise equal candidates within the visible batch. It also tolerates one inserted or
missing character or one adjacent transposition in a single title token of at
least five characters. Generated track searches apply the
same parenthetical simplification rather than issuing a second Spotify request.

The Spotify metadata used for these comparisons remains owned by the shared
`retune-spotify` catalog. The core library consumes supplied records and never
reads or persists that machine-local cache, so overlay and Last.fm decisions
remain deterministic and portable.
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

The complete candidate precedence, normalization, release/collection ranking,
ambiguity, and count-merge rules are documented in
[Last.fm import matching](lastfm-import-matching.md).

Release-shaped batches compare source and Spotify album and track titles with
the same normalized contained-token rules used by collection matching. Retune
automatically selects the uniquely strongest supported relation among candidates
that either map every source row one-to-one or cover at least 80% of both the
source rows and the Spotify album's distinct tracks; equally strong releases
remain unresolved. Clean Spotify supersets can therefore be selected despite
extra tracks. Artist credit is
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

Failed asynchronous apply jobs persist Spotify's supplied retry deadline with
the frozen job. Review shows that deadline in local time with a live countdown;
quota errors without a Spotify deadline say so instead of inventing one.
The importer state also projects the earliest active persisted Spotify cooldown,
so the same deadline remains visible without a selected failure and after relaunch.

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

Review batches are stable 1-based `ImportBatch` pages that preserve complete
source clusters without a row-count cap, and command arguments must identify
the requested batch and its artist/album identity. Row actions also require a
source ID; album-level actions can operate without one. Queue summaries cross the IPC boundary as
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
search terms refetch. Collection album cards independently choose which matched
albums are saved in full. Accept All prepares all remaining
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

Apply failures preserve a closed code (`spotify-rate-limited`,
`spotify-quota-exhausted`, or `apply-failed`), display-only message, and optional
retry deadline from the typed Spotify response through the persisted job, queue
view, and completion event. Cooldown persistence is best-effort and does not own
that classification. Legacy failed jobs without a known code retain their
message, deadline, frozen plan, stage, and attempt and project as `apply-failed`;
Retune never derives policy by parsing display text.

Collection applies materialize missing local records from the
persisted selected-album preview cache before the Spotify membership write.
Only targets absent from that cache fall back to individual Spotify metadata
reads, so retrying a frozen cached job does not repeat `/tracks/{id}` requests.
Both full-album and individual-track writes call the shared Spotify membership owner;
the importer does not depend on Tauri command implementations. Its account
identity recheck and remote membership write share the same owner-issued guard.

Spotify saved-track and saved-album memberships are not core-library state.
The shell keeps those account-scoped memberships separately while materializing
their Spotify tracks here for playback, ratings, and play counts. A materialized
track may therefore be individually saved, referenced by one or more saved
albums, both, or neither.

## Browse projection

The three-column browser is a pure projection over `Library`. `Selection`
contains source/category/artist/album filters; `Facets` and visible tracks are
derived from it. Facets deduplicate borrowed exact values and cache normalized
sort keys before owning their result strings. Choosing a broader facet clears invalid narrower selections.
When a resolved projection proves a preserved value stale, the UI falls back
hierarchically by clearing category, artist, and album for a missing category,
or artist and album for a missing artist or album.
Alternate views should consume the same library rather than introduce another
canonical model.

The UI keeps the last resolved projection visible while the same source and
facet selection is refreshed or its Library query changes. Source or facet
changes hide incompatible track rows until the new projection resolves. A
single-flight browse boundary admits one request at a time, replaces queued
work with the latest query, and ignores late responses. Spotify-scope typing
does not issue or invalidate a local browse; source, facet, and revision changes
still refresh the local projection used when Spotify search is closed. Category
prefix candidates depend on source, artist candidates on source and category,
and album candidates on source, category, and artist, so a focused pane can
finish a prefix while its own selection request is pending. Double-clicking a
facet row waits for that exact projection, then starts its first enabled visible
track with the enabled projection as the new queue.

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
