# Spotify integration

`retune-spotify` owns authentication, Web API transport, retry policy, and
normalization. The desktop provider composes it into sync, search, follows,
library membership, playlists, and playback activation.

## Authentication and tokens

Web API authentication uses Authorization Code with PKCE (S256), a loopback
redirect, state validation, and a bounded callback wait. Its grant covers the
library, playlist, and follow scopes needed by sync and browsing; `streaming` is
not a Web API requirement.

Built-in playback has a separate OAuth flow. It uses the current librespot
`SessionConfig` client ID, requests only `streaming`, and returns through the
same loopback listener at `/login`. The one-time access token is used to create
and verify a reusable librespot AP credential through login5, then only the
reusable credential is stored alongside the Web API token state. Web-token
refresh preserves it. A playback rejection clears only that credential; an
explicit Spotify disconnect clears the whole token record.

There is one shared `SpotifyClient`. Access-token refresh is coalesced behind a
refresh lock; a request that receives 401 refreshes once and retries once. Token
persistence is described in [Persistence](persistence.md).

## Request discipline

All Web API requests pass through the client's shared request gate. The gate
serializes the wait and send boundary so concurrent callers cannot release a
thundering herd when a cooldown expires.

Rate-limit behavior distinguishes two conditions:

- Transient 429: honor `Retry-After`, share the deadline, and retry at most three
  times. A client waits at most five minutes; longer waits return to the app.
- Quota exhaustion (`error.reason == "QUOTA_EXCEEDED"`): return a typed quota
  error immediately and do not blindly retry. Preserve a supplied deadline, but
  never invent one.

Content actions persist supplied `Retry-After` deadlines in the same cooldown
store so asynchronous import failures can show a local reset time and countdown.

5xx responses retry after one and three seconds before failing. The provider
records request counts and typed cooldowns by endpoint family; persisted
cooldowns prevent relaunch from immediately repeating a blocked request.

## Sync and caching

Saved tracks, albums, shows, episodes, and audiobooks are fetched sequentially.
Each successful batch is normalized and applied immediately; a later failure
leaves useful partial results. Core upsert preserves local overlay edits and
merges `added_at` using the earliest credible value. Spotify may return
alternate track URIs for the same album slot. The shell collapses matches with
the same artist, album, disc, track, title, duration, and release date during
sync and explicit album adds, preserving the existing overlay record. This
fallback cannot depend on `linked_from`, which Spotify no longer returns in
Development Mode track responses.

The shell records exact music membership in the account-scoped
`spotify-library.json` state: individually saved track URIs and saved album
records (including their membership time and materialized track URIs). A
complete `/me/tracks` plus `/me/albums` sync replaces those membership sets;
partial syncs leave the last complete exact state untouched and never prune.
An undecodable saved-track, saved-album, or album-track item makes the sync
partial even when Spotify returns the rest of its page, because a decoded
subset cannot prove exact absence. Explicit album actions likewise stop before
writing when album content is incomplete.
Complete reconciliation prunes only unreferenced Spotify music, so a track is
retained while any individual membership or saved album references it. Missing
or incomplete exact state keeps the legacy local-presence fallback for UI
membership flags. Explicit upstream removals also retain local records when
exact membership state is unknown or incomplete; a later complete sync is what
authorizes destructive reconciliation.

One async membership gate serializes complete sync snapshots with explicit
album/track saves and removals, preventing a stale snapshot or concurrent
command from overwriting a completed Spotify write. Replacing the Web API OAuth
token first resets persisted membership to unknown before replacement tokens
enter the shared token store, then exposes the new connection and queries
`/me`, so state from a previous account is never projected under new
credentials. A new Web OAuth grant also clears reusable playback credentials;
the user must explicitly authorize built-in playback for that account. Playback
authorization holds the same gate while comparing librespot's canonical
username with the connected Web API `/me` user ID, and refuses to persist a
credential minted for a different account.

Search album and track rows expose their respective exact membership as
`inLibrary` when known. Album-page DTOs keep `savedAlbum` separate from
`contentComplete`, expose album `addedAt`, and mark each track
`savedIndividually`; album rating eligibility follows content completeness,
not saved-album membership. Local track IDs remain available for rating and
playback even when an individual Spotify membership is absent.

Artist genres use an in-memory and persistent cache. Uncached artist lookups are
paced and capped per sync. Artist discography initially requests albums and
singles ten at a time; the UI explicitly loads later pages, preserves earlier
pages, and deduplicates requests.

## Search contract

Spotify search keeps one combined `artist,album,track` request and sends an
explicit offset with a limit of 10. This is the Development Mode maximum; the
UI paginates later offsets through the same `SpotifyClient` request gate, as
required by Spotify's [current search contract][spotify-search] and [migration
guidance][spotify-search-migration]. The desktop response exposes each result
group as `items`, `total`, and `nextOffset`. Type-restricted searches may omit
the other result groups; the shared client deserializes omitted groups as empty
pages so provider mapping remains uniform.

The search view stores successful pages by offset, merges every group returned
by a page, and deduplicates artists by Spotify ID and albums/tracks by URI.
Album and track rows carry exact membership flags from the local Spotify state
when available, otherwise the legacy local-presence fallback, so the explicit
Add action can render its current state without changing Spotify playback
behavior. Successful membership mutations are retained as search-level URI
overrides while navigating into album or artist pages, so returning to cached
results cannot restore stale action state.
Visible counts are transient UI state: All starts at five per group and a
filtered group starts at ten. Query changes discard pages; filter changes reset
visible counts but retain pages for the same query. A failed later page leaves
existing rows visible and can be retried for that group.

Last.fm source download and aggregation do not use Spotify or its account
gate. Opening a visible review batch lazily matches it through this same shared
client/request gate with official `album:`/`artist:` field filters and a limit
of 10, then fetches candidate tracks for set-overlap classification. Explicit
collection-album search passes the user's free-text or field-filter query through
the same provider and request gate; a search makes one album request and the
first Preview or direct Add fetches one album, while cached preview, add,
remove, and revisit operations make no request. An importer-wide async
lock serializes duplicate batch matches; cached revisits make no
matching/search request. After the visible batch resolves, React starts one
lookahead request for the next batch in the active sort order through that same
command and lock; it does not prefetch beyond that batch. A cached
Spotify-derived page trusts only an exact cached library identity; an inexact
identity resolves Spotify `/me` before the page is exposed. The first
successful match binds the session to Spotify `/me`; its final ownership check
and durable match mutation stay under the shared membership gate, and a later
identity mismatch suspends Spotify-derived work. Accept All is the explicit
sequential bulk exception: it sequentially prepares every remaining batch,
reports global unique album/track URI counts, and only then permits
confirmation and application. Review batches are stable persisted pages capped
at 100 source rows, so matching, fuzzy disclosures, and command source-ID
validation never widen to an adjacent batch.

## Writes and playlists

Overlay metadata never writes to Spotify. Explicit content actions may save or
remove library items, follow/unfollow artists, and create or mutate playlists.
Album save/remove actions send only the album URI to the generic library
endpoint; track save/remove actions send only track URIs. Album actions still
materialize album tracks locally, while membership state remains independent.
Any operation containing a local-file URI fails before an HTTP request.

Spotify is canonical for owned-playlist content. Reordering uses snapshot IDs to
detect concurrent changes, then reloads stale state. Retune does not request or
mutate item contents for playlists the current user does not own; it may display
their available metadata and cached counts.

The Last.fm importer reuses this shared client only for visible-batch matching
and explicit content acceptance. Automatic release matching uses the official
`album:`/`artist:` field filters with a limit of 10, then fetches candidate album
tracks for overlap classification; track rematching uses a direct `track:`
search. Import album acceptance calls the same reusable album operation as the
main UI and sends one album URI. Import track acceptance calls the reusable
track operation and sends only selected track URIs. The generic
`PUT /me/library` path is used; deprecated timestamped track-save endpoints are
not used.

## Contract changes

Spotify endpoints, scopes, quotas, and eligibility rules are external contracts.
Before changing them, verify current official Spotify documentation and cover
the transport policy with fake-response tests. Do not bypass the shared client
for a one-off endpoint.

## Compatibility/research record — 2026-08-11

This is an upstream compatibility change, not a correction to Retune's Web API
OAuth flow. Retune previously passed its developer-app access token directly to
a librespot session presenting Spotify's built-in client identity. That shortcut
was less idiomatic than persisting the reusable AP credential, but it worked on
2026-08-08. On 2026-08-10 login5
started returning `FaultyRequest(INVALID_CREDENTIALS)` while the same account
could still sync, browse, and search through the Web API. Music Assistant
reported the same ecosystem-wide [incident][ma-incident] and shipped [its fix][ma-fix]
that day:
Spotify now rejects a playback credential minted under an application's own
client ID when librespot presents Spotify's built-in client identity.

No corresponding Spotify announcement was found. The conclusion therefore
rests on Retune's logs, the independent Music Assistant incident and fix, and
the current librespot 0.8 authentication path. The remedy is to authorize
playback separately with librespot's current `SessionConfig` client ID, verify
the resulting reusable credential through login5, and retain Web API tokens
unchanged. This flow was also checked against Spotify's current [PKCE][spotify-pkce],
[scope][spotify-scopes], and [loopback redirect][spotify-redirect] guidance.
Login5 is an undocumented private protocol; its [librespot authentication
history][librespot-auth] is evidence, not an official contract. This boundary must be verified
again if Spotify or librespot changes it; do not collapse playback authorization
back into the Web API grant. No librespot version or fork change is required.

[spotify-pkce]: https://developer.spotify.com/documentation/web-api/tutorials/code-pkce-flow
[spotify-scopes]: https://developer.spotify.com/documentation/web-api/concepts/scopes
[spotify-redirect]: https://developer.spotify.com/documentation/web-api/concepts/redirect_uri
[ma-incident]: https://github.com/music-assistant/support/issues/6043
[ma-fix]: https://github.com/music-assistant/server/pull/5568
[librespot-auth]: https://github.com/librespot-org/librespot/pull/1309
[spotify-search]: https://developer.spotify.com/documentation/web-api/reference/search
[spotify-search-migration]: https://developer.spotify.com/documentation/web-api/tutorials/february-2026-migration-guide
