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

5xx responses retry after one and three seconds before failing. The provider
records request counts and typed cooldowns by endpoint family; persisted
cooldowns prevent relaunch from immediately repeating a blocked request.

## Sync and caching

Saved tracks, albums, shows, episodes, and audiobooks are fetched sequentially.
Each successful batch is normalized and applied immediately; a later failure
leaves useful partial results. Core upsert preserves local overlay edits.
Spotify may return alternate track URIs for the same album slot. The shell
collapses matches with the same artist, album, disc, track, title, duration, and
release date during sync and explicit album adds, preserving the existing
overlay record. This fallback cannot depend on `linked_from`, which Spotify no
longer returns in Development Mode track responses.

Artist genres use an in-memory and persistent cache. Uncached artist lookups are
paced and capped per sync. Artist discography initially requests albums and
singles ten at a time; the UI explicitly loads later pages, preserves earlier
pages, and deduplicates requests.

## Writes and playlists

Overlay metadata never writes to Spotify. Explicit content actions may save or
remove library items, follow/unfollow artists, and create or mutate playlists.
Any operation containing a local-file URI fails before an HTTP request.

Spotify is canonical for owned-playlist content. Reordering uses snapshot IDs to
detect concurrent changes, then reloads stale state. Retune does not request or
mutate item contents for playlists the current user does not own; it may display
their available metadata and cached counts.

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
