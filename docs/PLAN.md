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
  magic bytes. **Restore** replaces; **Merge** is additive, dedupe by `uri` (existing
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
