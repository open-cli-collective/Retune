# Retune — Architecture Plan (attempt-1)

Spec: `README.md` (design handoff) + `screenshots/` + `Retune.dc.html` (interaction reference only).

## Stack decision

**Tauri 2 + Rust workspace core + React/TypeScript/Vite frontend.**

Rationale: the core library is required to be Rust; Tauri exposes Rust commands to the
webview with no IPC boilerplate beyond `#[tauri::command]`, gives us the **native macOS
menu bar** (a spec requirement — File/Edit/View/Controls/Account/Help), system theme
detection, and platform-idiomatic app-data dirs. Electron would need a sidecar or NAPI
bindings to reach the Rust core; native AppKit forfeits the HTML prototype's direct
layout translation. React because the UI is a dense, state-driven single window with
derived views (progressive filter) — a good fit for props-down selectors, and the most
reliable target for LLM doer agents.

## Workspace layout

```
Cargo.toml               # workspace
crates/
  retune-core/           # PURE overlay domain. No I/O, no async, no Tauri, no HTTP.
  retune-spotify/        # Spotify Web API adapter (OAuth PKCE, library, search, player)
apps/desktop/
  src-tauri/             # shell: application layer (sync service, provider trait,
                         # fs persistence) + composition root wiring tauri commands,
                         # native menus, shortcuts
  src/                   # React UI (components + a thin store)
docs/PLAN.md             # this file
.github/workflows/ci.yml # fmt + clippy -D warnings + cargo test + tsc + vite build
```

## Boundaries (the load-bearing decisions)

1. **`retune-core` is a pure overlay library.** It owns: track records, album ratings,
   effective-rating precedence, genre normalization bookkeeping, merge/restore, serde.
   Records are layout-neutral data; the **three-column browse hierarchy lives in a
   separate `browse` projection module** (`browse::facets(&Library, &Selection)`,
   `browse::tracks(&Library, &Selection)`) — one consumer of the records, not the core
   API. An alternate layout (flat list, grid) is another projection module over the
   same records. Media types: each `Source` (music/podcasts/audiobooks) supplies a
   `LabelMap` naming the three generic facets (`cat/art/alb` per the spec); adding a
   media type = new source + label map.
2. **The provider boundary lives in the application layer, not core.**
   `MediaProvider` (async trait in `src-tauri`'s app module): fetch library snapshot
   per media kind, search artists/albums, playback control, add-to-library.
   `retune-spotify` exposes a concrete `SpotifyClient` and does **not** know the
   trait; the `impl MediaProvider for SpotifyClient` is written in `src-tauri`
   (trait is local there — no dependency cycle, no extra crate). Core never names
   Spotify; it consumes normalized records the sync service hands it. Tests
   substitute a fake provider.
3. **`OverlayStore`** (trait in the app layer, fs implementation beside it): load/save
   overlay JSON at the platform app-data dir. Atomic writes (temp file + rename).
   In-memory implementation for tests.
4. **Composition root is `src-tauri/main.rs`.** It constructs adapters, injects them,
   registers commands/menus. Nothing else constructs collaborators. No other traits —
   single-implementation helpers stay concrete.
5. **Frontend mirrors the split**: presentational components (Browser, TrackList,
   Transport, RatingStars, GetInfo, Prefs) take data + callbacks; one store module owns
   UI state (selections, search scope, theme, playing) and calls Tauri commands.
   Filtering logic lives in Rust, not duplicated in TS.

## Domain model (retune-core)

```rust
struct TrackRecord {
  id: TrackId,               // stable local id
  uri: String,               // provider URI, e.g. "spotify:track:…" — carries media
                             // kind; the dedupe key for merge and provider ops
  source: SourceId,          // music | podcasts | audiobooks
  cat: String,               // genre / category (overlay-normalized)
  art: String, alb: String, name: String,
  duration: Duration,
  rating: Option<Rating>,    // per-track override, 1..=5
  orig_cat: Option<String>,  // provider's original, recorded on first divergence
}
struct AlbumKey { source: SourceId, art: String, alb: String }
// Library holds Vec<TrackRecord> + BTreeMap<AlbumKey, Rating>
```

- **Album identity is deliberately the (source, artist, album) text tuple** — iTunes
  semantics, and the point of the overlay: editing a track's album text re-parents it
  to that group (leaving the old group's rating behind); normalizing two editions to
  one name merges their groups. This is the user's "full control" feature, not an
  anomaly. Consequence tests pin it: rename-out loses inherited rating; rename-into
  gains the target group's rating. (Reviewer preferred stable album entities; rejected
  as un-iTunes-like — text identity is the product behavior.)
- **Effective rating** = track override `??` album rating `??` unrated. Clicking a
  star always sets an explicit track override at that value — even if it equals the
  inherited value; clicking the star matching the current *explicit* override clears
  it (reverts to inherited).
- **Override marker "●"** = `orig_cat` is recorded and `cat != orig_cat`. Editing the
  genre back to the original hides the marker (equality check, not flag).
- **Progressive filter** = pure function over (library, `Selection { cat?, art?, alb? }`)
  in the `browse` module. Selecting a broader level resets narrower ones; enforced in
  `Selection` transition methods, tested.
- **Serialization**: versioned JSON envelope (`{"version":1, ...}`), serde. A
  `migrate(json) -> Library` entry point runs version upgrades; a fixture test pins
  that v1 files always load. Export `.json` / `.json.gz` (flate2); import sniffs gzip
  magic bytes. **Import validates `Library` invariants** — duplicate track ids or
  URIs are rejected as invalid (our own exports never produce them, so they mean
  corruption), and `next_id` is always recomputed above the max seen id so a stale
  value can never mint colliding ids. **Restore** replaces; **Merge** is additive, dedupe by `uri` (existing
  record wins, keeps its overlay edits). Corrupt file on startup → rename aside as
  `.corrupt-<ts>`, start empty, surface a notice — never silently overwrite.

## Spotify decisions (recorded so doers don't improvise)

- Auth: **OAuth 2 PKCE + loopback redirect** (`http://127.0.0.1:<port>/callback`).
  User-supplied Client ID (Spotify dev dashboard) entered once in Preferences, stored
  in app config. **Refresh token goes in the macOS Keychain** (`keyring` crate), never
  in config or the overlay file.
- **Feasibility gate (phase 0 of Spotify work, before any player code):** verify with
  the user's account that (a) they have Premium (Web API playback requires it),
  (b) a Development Mode app covers their use (owner + ≤5 users). Also re-verify
  current endpoint shapes against the Feb 2026 API changes — saved-library writes are
  URI-based (`/me/library`), not the removed `PUT /me/tracks`.
- **Sync covers every media kind**: saved tracks, saved albums (expanded to tracks),
  saved shows → episodes, **saved episodes (`/me/episodes`)**, saved audiobooks →
  chapters; paged with rate-limit backoff. **Trigger/cadence**: full reconciliation
  on app startup plus a manual File → "Sync from Spotify" item; periodic background
  sync is deferred until the manual path proves out. Reconciliation: provider
  snapshot vs overlay by `uri` — new URIs are added; URIs gone from Spotify are
  simply retained (overlay is the source of truth for the user's edits; we never
  silently drop their data, and no availability flag is modeled until a real need
  appears).
- **Search-to-library mapping**: search returns artists and albums. An *artist* row
  is navigation — it expands to that artist's albums (never a bulk import). An
  *album* row's "+ Add" fetches the album's track listing (paged) and adds every
  track. Where individual tracks are shown, they add singly. Every add also saves
  the corresponding URI to the user's Spotify library (the one write-back).
- Spotify tracks have **no genre**; artists do. Initial `cat` = first genre of the
  track's **first-listed (primary) artist**, else `"Unknown"`; artist lookups use
  **cached individual `/artists/{id}` requests** (the batch `?ids=` endpoint was
  removed in the Feb 2026 API changes) with bounded concurrency and backoff, cached
  per sync run.
- **Non-music facet mapping** (provider adapter follows this, never invents):
  *Episodes*: `cat` = show's category if the API supplies one else `"Uncategorized"`,
  `art` = show publisher (the podcaster), `alb` = show name, `name` = episode name.
  *Audiobook chapters*: `cat` = `"Uncategorized"` unless the API supplies a genre,
  `art` = first-listed author, `alb` = book name, `name` = chapter name. Multiple
  authors/publishers: first-listed wins, same rule as track artists.
- **Playback engine binds the concrete Spotify client deliberately.** The
  substitution boundary (`MediaProvider`) covers library/search/save — the paths
  tests actually substitute. A player port would today be a single-implementation
  interface, which this plan's own rules reject; extract one when a second playback
  backend (e.g. librespot) actually exists.
- **Sync cadence semantics**: Connect always performs the full first import (the
  product requirement). Startup reconciliation runs when connected and the
  "auto-add my entire Spotify library" preference is on (default **on**); manual
  File → Sync from Spotify always works. The preference means "keep pulling in
  what I add on Spotify automatically," nothing more.
- **Playback**: double-click establishes the playback context = the current filtered
  track list snapshot; next/prev navigate that snapshot via explicit play-track calls
  (never Spotify's own queue-next). Player state (playing/elapsed/current track) is
  **polled from `/me/player` as the authority** (~1s while playing) with optimistic UI
  updates in between; external changes (user touches Spotify directly) reconcile on
  next poll. **Natural track completion vs. external takeover**: when a poll shows a
  different item than expected, Retune advances its snapshot **only if the prior
  expected item was near completion** (elapsed within ~5s of duration at last poll);
  otherwise the user took control elsewhere — Retune clears its context and the LCD
  adopts/reflects the external playback. Spotify's own queue never advances Retune's
  context. A ~1-poll seam at track boundaries is accepted. Phase-5 checks include a
  natural-completion run and an external-takeover run. Target device = the user's
  logged-in desktop client.
- **Album artwork** is provider metadata, not overlay identity: the application
  layer's track/album DTOs carry an artwork URL (with a small on-disk cache later
  if needed); it never enters `retune-core` records.
- No play counts (spec: cut).

## Phases — each ends with a concrete check

| # | Deliverable | Verification |
|---|-------------|--------------|
| 1 | Workspace scaffold + CI | `cargo test` + `npm run build` green locally and in Actions; macOS `tauri build` compiles in CI |
| 2 | `retune-core`: model, browse projection, ratings, serde, merge/restore, migrations | unit tests written first; `cargo test -p retune-core`; includes album-reparenting, star-click, marker-equality, v1-fixture tests |
| 3 | Frontend UI on fixture data via Tauri commands | `tauri dev` renders; side-by-side vs `screenshots/playing-*.png`; filter/rating/theme interactions driven via computer use |
| 4 | Persistence + File menu (backup/export/restore/merge) + Get Info | round-trip test: export → restore → identical library; `.json` and `.json.gz` import; atomic-write + corrupt-file recovery tests (`OverlayStore` fs behavior lives here, not in core) |
| 5 | `retune-spotify`: feasibility gate, PKCE, per-kind sync, search, player context | feasibility checklist signed off by user; sync against live account; filtered-list next/prev behavior exercised |
| 6 | Shortcuts, zoom, prefs, polish | keyboard matrix from spec exercised via computer use |

Phases 1–4 run entirely on fixture data; phase 5 is the only step needing the user
(Client ID, Premium check, logged-in desktop app).

## Doer-agent split

Codex (`gpt-5.6-sol` medium) implements per-phase work orders with tests; smaller
models (`terra`/`luna`) take mechanical tasks (CI yaml, fixtures, docs). Claude is the
architect: writes/reviews every boundary, owns merges into `attempt-1`, and runs the
verification checks — doers never self-certify.

## Librespot playback backend (amendment, 2026-07-20)

User decision: embed librespot so Retune plays audio itself — no Spotify desktop
app required. Connect-based remote control stays as a fallback backend.

### Boundary (revised after architect review round 1)
`PlayerBackend` in the app layer with command surface
`play(snapshot, start_index) / toggle / next / prev / seek / set_volume / stop`.
Backends emit **backend-neutral events** on a channel; a single application
controller owns mapping them to `PlayerStateEvent` (local-id lookup from
snapshot URIs, error mapping, ordering, Tauri emission) so that policy is never
duplicated. Dispatch is a two-variant enum (`Backend::Connect | Backend::Local`),
not `dyn` — sidesteps async-trait object safety. Two impls:
- `ConnectBackend`: today's engine (snapshot queue, 1s poll authority, epoch
  guard, `resolve()` decision fn). Machinery is Connect-specific and stays here.
- `LocalBackend`: librespot Session + Player. Push events replace polling.
  Queue advance is our explicit call on `EndOfTrack` — same snapshot
  semantics, no Spotify queue.

### Auth (corrected)
librespot accepts our existing Web API access token via
`Credentials::with_access_token` — we reuse the hardened PKCE flow and the
existing token store wholesale. `librespot-oauth` is NOT used (its listener
drops OAuth state and has no timeout handling — a regression on our flow).
Scope additions to `auth::SCOPES`: `streaming` (session), `user-read-private`
(Premium preflight). Existing tokens lack these → switching the backend on
prompts one re-consent. No librespot credential cache at all (no plaintext
blob): a Session is created per run from our token, refresh stays in
`SpotifyClient`.

**process::exit is a library-wide hazard, patched before L-B**:
librespot-playback contains MULTIPLE `process::exit(1)` paths — non-Premium
sessions, sink failures, internal-state errors — any of which would terminate
Retune from a library thread; `Player::is_invalid` cannot observe a dead
process. Before the local backend is EXPOSED at all (L-B, not L-C), we carry a
maintained patch (git fork + `[patch.crates-io]`) that audits every reachable
`process::exit` in librespot-core/-playback and converts them to error
returns/events. L-A may run unpatched only because it is debug-gated and
disposable.

**Premium preflight is mandatory and fails closed**: `GET /me` product gates
Session creation, but `product` is deprecated — treat missing/unknown as NOT
premium (fail closed). Messaging: playback requires Premium on BOTH backends
(Connect playback is Premium-only too) — a free account gets one honest
error, not a fallback suggestion.

Token records gain a `scopes` field (serde default empty = legacy grant) so
the app can tell an old consent from an upgraded one; enabling the local
backend with a legacy token triggers the one re-consent. The refresh path
(client.rs `refresh_token` reconstructs the stored record) must carry
`stored.scopes` forward — the canonical requested scope set is written at
authorization and preserved verbatim on every refresh.

### Dependencies & lifecycle
- `librespot-core` + `librespot-playback` only (NOT the umbrella crate —
  default features pull discovery/libmdns), default-features off,
  `rodio-backend` + one TLS feature. MSRV: librespot 0.8 needs Rust 1.85 —
  bump the shell's `rust-version` (currently 1.77.2). All librespot types stay
  behind `LocalBackend`.
- A disconnected Session is dead: recreate on network loss/sleep-wake; tear
  down on backend switch; suppress late events from an old session via a
  generation counter (same pattern as the Connect epoch guard).
- Commands only *enqueue*: success ≠ playback. `Unavailable` reverses
  optimistic UI state; the player thread is supervised (`Player::is_invalid`)
  because rodio sink creation can panic inside it → surface `operation-error`.

### Event mapping (L-B contract)
Set `PlayerConfig.position_update_interval` (else `PositionChanged` never
fires). Map `SpotifyUri` back to the snapshot URI then to the snapshot's local
u64 id (never parse base62). Staleness is a TWO-stage filter with independent
counters: first the backend generation captured when the session was created
(drops events from torn-down sessions), then load-intent matching. The second
stage cannot be "latest `PlayRequestIdChanged` wins": `Player::load()` returns
BEFORE librespot assigns/emits the request id, so buffered old events could
slip between a new command and its id event (flicker, double-advance).
Instead, each queued load intent is bound to the next `PlayRequestIdChanged`
in FIFO order; events carrying an id belonging to a superseded intent are
discarded. All position-bearing values cross the controller boundary as
milliseconds and are converted there to the seconds `PlayerStateEvent` uses —
never assigned raw. Transition table (librespot event → emitted
`PlayerStateEvent`) applied only to events passing both filters:

| librespot event        | emitted state                                        |
|------------------------|------------------------------------------------------|
| Loading{uri, position_ms} | trackId=local(uri), elapsed=position_ms/1000, isPlaying preserves the requested intent (play vs paused-load) |
| Playing{uri, pos_ms}   | trackId=local(uri), elapsed=pos_ms/1000, isPlaying=true |
| Paused{uri, pos}       | same but isPlaying=false                             |
| PositionChanged{pos_ms}| elapsed=pos_ms/1000, playing state unchanged         |
| Seeked{pos_ms} / PositionCorrection{pos_ms} | elapsed=pos_ms/1000, unchanged  |
| Unavailable{uri}       | `operation-error` for the track, then advance the queue (identical to EndOfTrack) — optimistic UI state reverts to the next track or empty |
| Stopped                | empty player state                                   |
| EndOfTrack             | backend advances the queue FIRST (loads next, same generation); no intermediate empty emission — the next Loading/Playing drives the UI; queue exhausted → empty state |

Reducer tests cover: stale-generation discard, superseded-intent discard
under rapid A→B loads with a buffered old EndOfTrack (no flicker, no
double-advance), Unavailable advance + error, Stopped clear, natural advance
ordering (no flicker-empty between tracks), Loading position preservation,
PositionChanged ticks, and ms→seconds conversion on every position-bearing
event.
Volume: Retune 0..=100 → softmixer 0..=65535, initial volume applied from
settings and persisted on change; soft volume attenuates in-app audio, not
macOS system volume.

### Media coverage limit
librespot plays track and episode URIs. Audiobook chapter URIs are playable on
NEITHER backend as documented (`/me/player/play` documents track URIs and
album/artist/playlist contexts; Retune stores `spotify:chapter:` URIs, and the
current Connect path attempting them is unverified). Empirically verify
chapter playback via Connect at the live gate; until proven, audiobook
playback is rejected on both backends with an honest message. Local files:
out of scope entirely (librespot needs configured directories and paths our
snapshot doesn't carry).

### Selection & risk posture
`Settings.playback_backend: "connect" | "local"`, serde default `connect` so
existing settings files upgrade silently. Switching to local is atomic and
fail-safe: preflight (scopes, Premium) and create the local session FIRST;
only on success stop the Connect backend and persist the selection; any
failure leaves Connect running and the setting unchanged. Tests: missing
field defaults to connect; failed switch retains connect.

librespot is a reverse-engineered protocol Spotify does not sanction: auth
paths have broken before and account risk is nonzero — this is why Connect
fallback is retained and why the toggle is explicit in Preferences.
Credentials may be revoked server-side at any time; the backend must degrade
to a visible error, never a crash.

### Phases
- L-A Spike: scope additions + re-consent → Premium preflight → Session from
  our access token → play one hardcoded URI to default output, behind a
  debug-only menu item. Proves auth + audio on macOS. Record clean release
  build time and bundle-size delta; audio cache disabled.
- L-B Full LocalBackend per the contracts above; Preferences toggle; Connect
  remains default. Empirical matrix: sleep/wake, no output device, output
  device switch — in `tauri dev` AND a packaged build.
- L-C Default flip to local once L-B is verified empirically; Connect stays
  selectable.
