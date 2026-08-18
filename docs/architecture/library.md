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
counts use one session-persisted Sum, highest-played spelling Overwrite, or Zero
decision per collapsed Spotify track target, then local play count takes the maximum of current and historical
values. `last_played_at` takes the latest relevant scrobble and `added_at` takes
the earliest; Zero never erases known Retune plays. Blank genre/rating options
are no-ops. Whole-album rating is stored as an album rating, while
selected-track mode stores explicit track ratings only.

Each page has two independent intents: import Spotify content and include
historical play counts. Both default on, at least one must remain on, and
whole-album is a separate page-level content mode defaulting off. Content-only
acceptance saves membership and applies source `added_at` without changing
plays or `last_played_at`; counts-only performs no Spotify write and updates
only already-materialized matched Retune tracks.

The importer’s “Show Spotify search terms” preference is session-level and is
restored on resume. Fuzzy disclosures are bounded to the visible persisted
`ImportBatch`; the target-wide count decision still includes completed source
rows from other batches.

The source importer is V2. It records a fixed profile-bound `historyTo`, probes
Last.fm metadata once, downloads pages at the documented 200-row limit from
oldest toward page 1, and stores parsed raw pages under a snapshot-specific
machine cache. Rows at or after `historyTo` are rejected before caching or
counting; the exact Last.fm username is recorded in the manifest and each page.
The manifest is authoritative: an orphan page file is harmless and may be
overwritten, while an acknowledged missing, corrupt, oversized, or
metadata-mismatched page quarantines the whole snapshot and restarts V2. A
retryable Last.fm failure is persisted and retried in-process at the capped
backoff without advancing the cursor. No aggregation happens until every page
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

Review batches are stable 1-based `ImportBatch` pages capped at 100 source rows.
Normal albums under the cap remain one batch; larger albums and singles split
deterministically, and command arguments must identify the requested batch and
its source rows. Queue summaries cross the IPC boundary as bounded cursor/limit
pages with `sourceCount`, not source IDs; Accept All applies its prepared batches
sequentially and refreshes the queue once after the bulk operation.

Spotify matching is lazy. Opening a visible batch uses the shared client and
request gate, serializes duplicate requests, binds the Spotify account on the
first match, and caches the result; reopening it makes no API call. Accept All
is the only bulk exception and prepares remaining batches sequentially before
showing global unique album/track URI counts and awaiting confirmation. Excluded rows remain
source-history decisions and can be undone before acceptance; they never remove
a track inherently materialized by a saved whole album.

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
