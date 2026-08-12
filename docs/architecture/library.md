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
  It preserves overlay name/artist/album/rating, play history, and added time.
- Adding or merging deduplicates by URI. Existing overlay values win.
- Provider refresh preserves each track's playback-enabled overlay value.
- Restoring replaces the library after validating the imported envelope.

Overlay edits never mutate source-file tags or Spotify metadata.

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
