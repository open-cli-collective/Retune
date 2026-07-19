# Handoff: Retune — iTunes-style Spotify wrapper with a local metadata overlay

## Overview
Retune is a desktop app that wraps the Spotify desktop client and re-imposes the
circa-2004 **iTunes three-column browsing model** (Genre → Artist → Album →
Tracks, a progressive filter) on top of a user's Spotify library. Its defining
feature is a **local metadata overlay**: the user's own normalized tags, ratings,
and grouping that map onto Spotify track IDs without ever mutating anything on
Spotify's backend. The overlay is the point — it restores album-first, no-playlist
listening for people who hate playlist-centric modern Spotify.

The bundled prototype is a working demonstration of the **UX layout and
interaction model**, not the architecture. Read the "Product intent" section for
the engineering the user actually wants built.

## About the Design Files
`Retune.dc.html` in this bundle is a **design reference created in HTML** — a
prototype showing intended look and behavior. It is **not production code to copy
directly**. The task is to recreate this design's layout and interactions in a
real desktop app, using the framework chosen below and idiomatic patterns for it.
The HTML uses a small in-house component runtime (`support.js`); ignore that
runtime entirely — it is a prototyping tool, not part of the deliverable.

## Fidelity
**High-fidelity for layout and interaction; neutral on chrome styling.** The
column structure, progressive-filter behavior, rating inheritance, search-scope
toggle, menu organization, and overall information architecture are the spec and
should be recreated faithfully. The *visual chrome* (exact gradients, the faux
macOS window frame, traffic lights) is deliberately generic — the user explicitly
does not want brushed metal or specific button textures reproduced. Match native
platform conventions for chrome instead.

## Product intent (from the user — this drives the build, not just the mock)

**Workflow / process**
- Follow the **planning portion of the plan-execute-TDD workflow** before writing
  code: produce a plan, then execute test-first.
- **Idiomatic SOLID.** Keep the overlay/domain logic decoupled from Spotify I/O
  and from the UI so future layouts and future media types drop in without
  rewrites (see "Extensibility" — the layout is explicitly "one *possible*
  layout").
- **GitHub Actions CI/CD** from the start.
- The core library should be **Rust**, with its capabilities exposed through
  natural menu / menu-bar items in the UI. (Framework for the shell is open —
  Electron, Tauri, or native macOS are all acceptable. Tauri pairs naturally with
  a Rust core; call this out in the plan and let the user confirm.)
- Set up **computer-use / local testing** for the Spotify-dependent paths, or
  give the user step-by-step instructions to do so.

**Spotify integration rules**
- The user must have the **Spotify desktop app installed and be logged in** — that
  is acceptable.
- **The overlay never writes back to Spotify.** Overlay data (custom genre/artist/
  album tags, ratings) is local-only. "Read-only" refers to Spotify's backend, not
  to the overlay itself — the overlay is fully user-editable locally.
- **Everything in the user's Spotify library populates the local library.** Using
  Spotify's own "Add to library" should also add to the local overlay library.
- **Play counts**: librespot cannot expose Spotify play counts, so this feature
  was cut — there is no Plays column and no play-count settings. (If a future
  backend can supply them, reintroduce a Plays column + a Preferences section.)

**Persistence & portability**
- Persistence should be **platform-idiomatic** (app data dir, etc.), with the
  overlay serializable to **JSON**.
- **Backup / Export**: export to `.json` and to `.json.gz`.
- **Import** must accept either `.json` or `.json.gz`.
- **Restore** = replace the current library. **Merge** = additive import
  (dedupe by Spotify track ID). Both belong in the File menu.

## Screens / Views

The app is a **single window** with fixed regions. There is one primary view; the
only modal is Get Info.

### Main window
Fixed layout, top to bottom:

1. **Title bar** (34px) — window chrome + centered title "Retune — Library" +
   a theme toggle (☾/☀) on the right.
2. **Menu bar** (24px) — Retune · **File** (functional) · Edit · View · Controls ·
   Account · Help. On native macOS this is the system menu bar, not an in-window
   strip; the strip in the mock only exists to show where File items live.
3. **Transport bar** (60px) — 3-column grid `200px | 1fr | 268px`:
   - Left: prev / play-pause / next, volume icon + slider.
   - Center: an "LCD" now-playing readout — track title, `artist — album`,
     elapsed time, progress bar, remaining time.
   - Right: **search scope toggle** (Library / Spotify pills) + search field.
4. **Body** — 2-column grid `190px | 1fr`:
   - **Sidebar** (190px): "Library" section listing the media-type sources
     (Music / Podcasts / Audiobooks) with per-source counts; a "Playlists"
     section (placeholder); a footer note about the overlay staying local.
   - **Content**: the three-column browser, an optional album-rating strip, the
     track list, and a status bar (see below).
5. **Status bar** (26px) — "+" add button (left), "N songs, H:MM hours" (center),
   "N overlay edits" (right).

### Three-column browser (the core of the UX)
Fixed height ~200px, 3 equal columns: **Genre | Artist | Album** (labels change
per media type — see Extensibility). Each column: a 20px header + a scrollable
list. Each list has an "All (N …)" row at top that clears that filter level.

**Progressive filter behavior (critical):**
- Selecting a Genre filters the Artist column to artists having tracks in that
  genre, and filters everything downstream.
- Selecting an Artist filters the Album column and the track list.
- Selecting an Album filters the track list to that album.
- Selecting a narrower level never clears a broader one; selecting a broader level
  resets the narrower selections (artist/album/track).
- The track list always reflects the current intersection of selections.

### Album-rating strip (conditional)
Appears (28px) between the browser and the track list **only when an album is
selected**. Shows the album name, a 5-star control that sets the album-level
rating, and a hint: "applies to all tracks unless individually overridden."

### Track list
Header row + rows. Grid columns:
`22px | Name (minmax 160px,1.6fr) | Time 52px | Artist 1.1fr | Album 1.1fr | Genre 0.9fr | Rating 84px`.
- Col 1: playing indicator (▶ playing / ❚❚ paused) on the current track.
- Genre cell shows a leading "●" when the track's genre was overridden from
  Spotify's original.
- Rating: 5 stars. **Effective rating = track override ?? album rating ?? unrated.**
  Inherited stars render gray/muted; explicit overrides render gold (#e0a53d);
  empty stars are faint. Clicking a star sets a track override; clicking the
  currently-set star clears the override (reverts to inherited).
- Row selection highlights in the accent color; double-click plays.
- Zebra striping on alternate rows (toggleable).

### Get Info modal
Opened via the ⓘ affordance, File → Get Info, or ⌘I on the selected track.
Shows: read-only **Spotify ID**;
editable **Name / Artist / Album / Genre** fields (genre with the hint
'normalize freely, e.g. "Operatic Rock" → "Rock"'); a **track rating** star
control with inherited/override/clear semantics; an info banner that, when the
track's genre differs from Spotify's, reads 'Spotify reports this as "…". Your
overlay wins in Retune.' Buttons: Cancel / **Save Overlay**.

## Interactions & Behavior
- **Search scope toggle**: Library scope filters the local library in place.
  Spotify scope + a non-empty query hides the local track list and shows a global
  Spotify results area (stubbed in the mock) with a "+ Add to Library pulls the
  track and its metadata into your overlay" affordance. **Decision: Spotify-scope
  search focuses on artist + album results** (mirroring how Spotify's own search
  works today — clunky but familiar), not the full Genre/Artist/Album browser.
  Present results grouped by artist and album; adding any result pulls the track
  and its metadata into the local overlay library.
- **Rating inheritance (confirmed precedence)**: effective rating = per-track
  override, else album rating, else unrated. Album rating cascades to its tracks;
  per-track overrides win; re-clicking the set star clears the override.
- **Genre normalization**: editing a track's genre records the Spotify original
  the first time it diverges, and flags the track with "●".
- **Playback (prototype-only)**: transport updates the LCD and advances a timer;
  in the real app this drives / reflects the Spotify player.
- **Keyboard shortcuts**: Space = play/pause, ← / → = prev/next track (within the
  current filtered list), ⌘I = Get Info on selected track, ⌘L = focus Library
  scope, Esc = close modal/menu. Expand as appropriate for the platform.
- **File menu**: Get Info · Preferences (⌘,) · Add to Library from Spotify ·
  Back Up (.json) · Export (.json.gz) · Restore · Merge. The "Retune" app menu
  also opens Preferences.
- **UI zoom / text size**: Cmd/Ctrl + `=`/`-` to grow/shrink, Cmd/Ctrl + `0` to
  reset, and Cmd/Ctrl + scroll wheel. Implemented as a whole-window zoom factor
  (clamped ~0.7–1.8) so text and rows scale together; wire an equivalent into the
  View menu in the real app. Native platforms may prefer a dynamic-type / text-
  size setting instead of CSS zoom.
- **Preferences modal** (⌘, / Retune menu): Library section — "auto-add my entire
  Spotify library". (A Play Counts section was removed — see Play counts above.)
- **Theme**: light, dark, and **system** (follows OS `prefers-color-scheme`, live-
  updating). Cycled via the ☀/☾/🖥 icon in the title bar and the View menu; also a
  prop. (Original iTunes had no dark mode; the user wants one anyway.)

## State Management
Domain state (belongs in the Rust core / a UI-agnostic store):
- `library`: per media type (`music` / `podcasts` / `audiobooks`), a list of
  overlay track records: `{ id, spotifyId, cat, art, alb, name, time,
  rating|null, orig|null }` where `cat/art/alb` are the generic
  category/artist/album fields (see Extensibility), `rating` is a per-track
  override or null, and `orig` records the pre-normalization genre or null.
- `albumRatings`: per media type, a map keyed by `artist␞album` → 1–5.

UI state (belongs in the shell):
- `source` (active media type), `genre` / `artist` / `album` (filter selections),
  `selectedTrack`, `playing` / `isPlaying` / `elapsed`, `query` / `searchScope`,
  `theme`, and modal/menu open flags.

## Extensibility (do not design into a corner)
The mock stores tracks with **generic fields `cat / art / alb`** and a per-source
**label map** that renames the columns per media type:
- Music → Genre / Artist / Album (track = Song)
- Podcasts → Category / Podcaster / Show (track = Episode)
- Audiobooks → Category / Author / Book (track = Chapter)

Adding a media type is a new source + label map, **not a new layout**. The
three-column browser is one *possible* projection of the overlay; keep the overlay
model and the layout decoupled so alternate layouts (e.g. a flat search list, a
grid) can consume the same data. This decoupling is an explicit user requirement.

## Design Tokens
Layout is the spec; exact chrome colors are illustrative (match platform native).
- **Accent (selection)**: `#3f7fd6` (also a configurable prop in the mock).
- **Star colors**: explicit/override `#e0a53d` (gold); inherited `rgba(150,150,150,.7)`;
  empty `rgba(150,150,150,.4)`.
- **Override marker**: leading "●" in the genre cell.
- **Light theme** (illustrative): desk `#b9bcc2`, list `#ffffff`, zebra alt
  `#edf3fe`, text `#1a1a1a`, borders `#b7b7b7`, headers `#f0f0f0→#e2e2e2`,
  sidebar `#e6ebf2→#dbe2ec`, LCD `#eef2f7→#e2e8f0`.
- **Dark theme** (illustrative): desk `#161719`, list `#1f2124`, zebra alt
  `#26282d`, text `#e9e9ec`, borders `#45474c`, headers `#35373c→#2c2e32`.
- **Type**: dense UI text, 11–12.5px; system font (Lucida Grande / system-ui).
  Section labels 10px uppercase, letter-spacing .04em.
- **Row heights**: browser rows 17px, track rows 18px, column headers 20px.

## Screenshots
Each state is provided in both light and dark themes (`screenshots/`):
- `playing-light.png` / `playing-dark.png` — track playing (LCD populated, ▶ row marker).
- `menu-light.png` / `menu-dark.png` — File menu open.
- `viewmenu-light.png` / `viewmenu-dark.png` — View menu (text size + theme).
- `getinfo-light.png` / `getinfo-dark.png` — Get Info overlay editor.
- `prefs-light.png` / `prefs-dark.png` — Preferences.
Toggle themes via the ☀/☾/🖥 icon in the title bar, the View menu, or the theme prop.

## Assets
None. No images, icons, or external fonts — glyphs are Unicode (▶ ❚❚ ⏮ ⏭ ★ ☆ ●
🔊 ♪ 🎙 📖 🔒 ⌕). Album art is a striped placeholder in the mock; the real app
should use Spotify artwork.

## Files
- `screenshots/` — light + dark reference renders (playing, File menu, View menu, Get Info, Preferences).
- `Retune.dc.html` — the full interactive prototype (layout, progressive filter,
  ratings + inheritance, search-scope toggle, theme, JSON/JSON.gz backup/restore/
  merge, Get Info, keyboard shortcuts). Open in a browser to interact.
