# Library domain

`retune-core` is Retune's deterministic domain layer. It contains no filesystem,
network, async, Tauri, or UI code. Callers provide data and persist the result.

## Records and identity

`TrackId` is a stable local integer. A track's provider URI is its deduplication
identity; local imports use canonical `file://` URIs. A record contains its
source, category/artist/album/name overlay, duration, track/disc numbers, rating,
play count, timestamps, kind, bitrate, and original provider category.

Album identity is deliberately `(source, artist text, album text)`. Editing an
artist or album therefore re-parents a track, and normalizing two editions to the
same text merges their Retune album group.

## Overlay rules

- Track rating overrides album rating; absent track rating inherits the album.
- The first category divergence records the provider category. The override
  marker is shown only while current and original categories differ.
- Provider upsert refreshes track/disc, kind, bitrate, and provider category.
  It preserves overlay name/artist/album/rating, play history, and added time.
- Adding or merging deduplicates by URI. Existing overlay values win.
- Restoring replaces the library after validating the imported envelope.

Overlay edits never mutate source-file tags or Spotify metadata.

## Browse projection

The three-column browser is a pure projection over `Library`. `Selection`
contains source/category/artist/album filters; `Facets` and visible tracks are
derived from it. Choosing a broader facet clears invalid narrower selections.
Alternate views should consume the same library rather than introduce another
canonical model.

## Serialization

Core exports use a versioned JSON envelope and optionally gzip at the application
boundary. Import rejects duplicate IDs and URIs and recomputes the next local ID.
Application backup adds settings and playlist cache data around the core
envelope; see [Persistence](persistence.md).

## Change guidance

Keep provider translation out of this crate. New behavior belongs here only when
it can be expressed and tested as a deterministic transformation of library
state.
