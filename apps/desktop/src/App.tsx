import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { useEffect, useReducer, useRef, useState } from 'react'
import './App.css'

type Source = 'music' | 'podcasts' | 'audiobooks'
type Theme = 'light' | 'dark' | 'system'
type Selection = { cat?: string; art?: string; alb?: string }

type Track = {
  id: number
  name: string
  art: string
  alb: string
  cat: string
  durationSecs: number
  overridden: boolean
  rating: { stars: number; explicit: boolean } | null
}

type BrowseView = {
  facets: { cats: string[]; arts: string[]; albs: string[] }
  tracks: Track[]
  albumRating: number | null
  counts: {
    tracks: number
    totalSecs: number
    overlayEdits: number
    perSource: Record<Source, number>
  }
}

type TrackInfo = {
  id: number
  uri: string
  source: Source
  name: string
  art: string
  alb: string
  cat: string
  origCat: string | null
  rating: { stars: number; explicit: boolean } | null
  inheritedRating: number | null
  genres: string[]
}

const emptyTracks: Track[] = []

type State = {
  source: Source
  sel: Selection
  query: string
  scope: 'library' | 'spotify'
  selectedTrackId?: number
  playing: { trackId: number; elapsed: number; isPlaying: boolean } | null
  theme: Theme
  systemDark: boolean
  view: BrowseView | null
  revision: number
  error?: string
  notice?: string
  info?: TrackInfo
}

type Action =
  | { type: 'view'; view: BrowseView }
  | { type: 'error'; error: string }
  | { type: 'source'; source: Source }
  | { type: 'select'; facet: keyof Selection; value?: string }
  | { type: 'query'; query: string }
  | { type: 'scope'; scope: State['scope'] }
  | { type: 'selectTrack'; id: number }
  | { type: 'play'; id: number }
  | { type: 'togglePlay' }
  | { type: 'step'; id: number }
  | { type: 'tick'; duration: number; nextId: number }
  | { type: 'theme' }
  | { type: 'systemTheme'; dark: boolean }
  | { type: 'refresh' }
  | { type: 'notice'; notice?: string }
  | { type: 'info'; info?: TrackInfo }

const initialState: State = {
  source: 'music',
  sel: {},
  query: '',
  scope: 'library',
  playing: null,
  theme: 'system',
  systemDark: false,
  view: null,
  revision: 0,
}

function reducer(state: State, action: Action): State {
  switch (action.type) {
    case 'view':
      return { ...state, view: action.view, error: undefined }
    case 'error':
      return { ...state, error: action.error }
    case 'source':
      return { ...state, source: action.source, sel: {}, query: '', selectedTrackId: undefined }
    case 'select': {
      const sel = action.facet === 'cat'
        ? { cat: action.value }
        : action.facet === 'art'
          ? { cat: state.sel.cat, art: action.value }
          : { ...state.sel, alb: action.value }
      return { ...state, sel, selectedTrackId: undefined }
    }
    case 'query':
      return { ...state, query: action.query, selectedTrackId: undefined }
    case 'scope':
      return { ...state, scope: action.scope }
    case 'selectTrack':
      return { ...state, selectedTrackId: action.id }
    case 'play':
      return { ...state, selectedTrackId: action.id, playing: { trackId: action.id, elapsed: 0, isPlaying: true } }
    case 'togglePlay':
      return state.playing
        ? { ...state, playing: { ...state.playing, isPlaying: !state.playing.isPlaying } }
        : state
    case 'step':
      return { ...state, selectedTrackId: action.id, playing: { trackId: action.id, elapsed: 0, isPlaying: true } }
    case 'tick':
      if (!state.playing?.isPlaying) return state
      return state.playing.elapsed + 1 >= action.duration
        ? { ...state, playing: { trackId: action.nextId, elapsed: 0, isPlaying: true } }
        : { ...state, playing: { ...state.playing, elapsed: state.playing.elapsed + 1 } }
    case 'theme': {
      const theme = state.theme === 'system' ? 'light' : state.theme === 'light' ? 'dark' : 'system'
      return { ...state, theme }
    }
    case 'systemTheme':
      return { ...state, systemDark: action.dark }
    case 'refresh':
      return { ...state, revision: state.revision + 1 }
    case 'notice':
      return { ...state, notice: action.notice }
    case 'info':
      return { ...state, info: action.info }
  }
}

const labels = {
  music: { facets: ['Genre', 'Artist', 'Album'], item: 'song', icons: '♪', name: 'Music' },
  podcasts: { facets: ['Category', 'Podcaster', 'Show'], item: 'episode', icons: '🎙', name: 'Podcasts' },
  audiobooks: { facets: ['Category', 'Author', 'Book'], item: 'chapter', icons: '📖', name: 'Audiobooks' },
} as const

function formatTime(seconds: number) {
  const hours = Math.floor(seconds / 3600)
  const minutes = Math.floor((seconds % 3600) / 60)
  const secs = Math.floor(seconds % 60)
  return hours
    ? `${hours}:${String(minutes).padStart(2, '0')}:${String(secs).padStart(2, '0')}`
    : `${minutes}:${String(secs).padStart(2, '0')}`
}

function App() {
  const [state, dispatch] = useReducer(reducer, initialState)
  const view = state.view
  const tracks = view?.tracks ?? emptyTracks
  const openInfo = (id?: number) => {
    if (id === undefined) return
    invoke<TrackInfo>('get_track', { id })
      .then((info) => dispatch({ type: 'info', info }))
      .catch((error) => dispatch({ type: 'error', error: String(error) }))
  }

  useEffect(() => {
    let active = true
    invoke<BrowseView>('browse', {
      source: state.source,
      sel: state.sel,
      query: state.scope === 'library' && state.query.trim() ? state.query : undefined,
    }).then((next) => active && dispatch({ type: 'view', view: next }))
      .catch((error) => active && dispatch({ type: 'error', error: String(error) }))
    return () => { active = false }
  }, [state.source, state.sel, state.query, state.scope, state.revision])

  useEffect(() => {
    invoke<string | null>('startup_notice')
      .then((notice) => dispatch({ type: 'notice', notice: notice ?? undefined }))
      .catch((error) => dispatch({ type: 'error', error: String(error) }))
  }, [])

  useEffect(() => {
    const unlisten = listen('get-info', () => openInfo(state.selectedTrackId))
    return () => { void unlisten.then((stop) => stop()) }
  }, [state.selectedTrackId])

  useEffect(() => {
    const changed = listen('library-changed', () => dispatch({ type: 'refresh' }))
    const failed = listen<string>('operation-error', ({ payload }) => dispatch({ type: 'error', error: payload }))
    return () => {
      void changed.then((stop) => stop())
      void failed.then((stop) => stop())
    }
  }, [])

  useEffect(() => {
    const media = window.matchMedia('(prefers-color-scheme: dark)')
    const sync = () => dispatch({ type: 'systemTheme', dark: media.matches })
    sync()
    media.addEventListener('change', sync)
    return () => media.removeEventListener('change', sync)
  }, [])

  useEffect(() => {
    document.documentElement.dataset.theme = state.theme === 'system'
      ? state.systemDark ? 'dark' : 'light'
      : state.theme
  }, [state.theme, state.systemDark])

  useEffect(() => {
    if (!state.playing?.isPlaying) return
    const currentIndex = tracks.findIndex((track) => track.id === state.playing?.trackId)
    const current = tracks[currentIndex]
    if (!current) return
    const next = tracks[(currentIndex + 1) % tracks.length]
    const timer = window.setInterval(() => {
      dispatch({ type: 'tick', duration: current.durationSecs, nextId: next.id })
    }, 1000)
    return () => window.clearInterval(timer)
  }, [state.playing?.trackId, state.playing?.isPlaying, tracks])

  const mutate = (command: string, args: Record<string, unknown>) => {
    invoke(command, args)
      .then(() => dispatch({ type: 'refresh' }))
      .catch((error) => dispatch({ type: 'error', error: String(error) }))
  }
  const step = (direction: number) => {
    if (!tracks.length) return
    const index = tracks.findIndex((track) => track.id === state.playing?.trackId)
    const next = tracks[(index < 0 ? 0 : index + direction + tracks.length) % tracks.length]
    dispatch({ type: 'step', id: next.id })
  }
  const playingTrack = tracks.find((track) => track.id === state.playing?.trackId)

  return (
    <main className="app-shell">
      <TransportBar
        playing={state.playing}
        track={playingTrack}
        query={state.query}
        scope={state.scope}
        theme={state.theme}
        onQuery={(query) => dispatch({ type: 'query', query })}
        onScope={(scope) => dispatch({ type: 'scope', scope })}
        onPlay={() => dispatch({ type: 'togglePlay' })}
        onPrev={() => step(-1)}
        onNext={() => step(1)}
        onTheme={() => dispatch({ type: 'theme' })}
      />
      <div className="body-grid">
        <Sidebar state={state} onSource={(source) => dispatch({ type: 'source', source })} />
        <section className="content">
          <BrowserPane state={state} onSelect={(facet, value) => dispatch({ type: 'select', facet, value })} />
          {state.sel.alb && (
            <AlbumRatingStrip
              album={state.sel.alb}
              rating={view?.albumRating ?? null}
              onRate={(stars) => mutate('set_album_rating', {
                source: state.source,
                art: state.sel.art ?? tracks[0]?.art ?? '',
                alb: state.sel.alb,
                stars,
              })}
            />
          )}
          {state.notice && <div className="startup-notice"><span>{state.notice}</span><button aria-label="Dismiss notice" onClick={() => dispatch({ type: 'notice' })}>×</button></div>}
          {state.scope === 'spotify' ? (
            <div className="spotify-stub">Spotify search arrives with the Spotify connection — Phase 5</div>
          ) : (
            <TrackList
              tracks={tracks}
              label={labels[state.source]}
              selectedId={state.selectedTrackId}
              playing={state.playing}
              onSelect={(id) => dispatch({ type: 'selectTrack', id })}
              onPlay={(id) => dispatch({ type: 'play', id })}
              onRate={(id, stars) => mutate('click_track_star', { id, stars })}
              onInfo={openInfo}
            />
          )}
          {state.error && <div className="error-banner">{state.error}</div>}
          <StatusBar view={view} unit={labels[state.source].item} />
        </section>
      </div>
      {state.info && <GetInfo key={state.info.id} track={state.info} onCancel={() => dispatch({ type: 'info' })} onSaved={() => {
        dispatch({ type: 'info' })
        dispatch({ type: 'refresh' })
      }} onError={(error) => dispatch({ type: 'error', error })} />}
    </main>
  )
}

function TransportBar({ playing, track, query, scope, theme, onQuery, onScope, onPlay, onPrev, onNext, onTheme }: {
  playing: State['playing']; track?: Track; query: string; scope: State['scope']; theme: Theme
  onQuery: (query: string) => void; onScope: (scope: State['scope']) => void
  onPlay: () => void; onPrev: () => void; onNext: () => void; onTheme: () => void
}) {
  const elapsed = playing?.elapsed ?? 0
  const duration = track?.durationSecs ?? 0
  return <header className="transport">
    <div className="transport-controls">
      <button aria-label="Previous track" onClick={onPrev}>◀◀</button>
      <button className="play-button" aria-label={playing?.isPlaying ? 'Pause' : 'Play'} onClick={onPlay}>{playing?.isPlaying ? '❚❚' : '▶'}</button>
      <button aria-label="Next track" onClick={onNext}>▶▶</button>
      <span aria-hidden="true">🔊</span><input aria-label="Volume" type="range" min="0" max="100" defaultValue="62" />
    </div>
    <div className="lcd">
      <div className="lcd-copy"><strong>{track?.name ?? 'Retune'}</strong><span>{track ? `${track.art} — ${track.alb}` : 'Not Playing'}</span></div>
      <div className="progress-row"><time>{track ? formatTime(elapsed) : '—:—'}</time><progress max={duration || 1} value={elapsed} /><time>{track ? `-${formatTime(Math.max(0, duration - elapsed))}` : ''}</time></div>
    </div>
    <div className="search-area">
      <div className="scope-pills" aria-label="Search scope">
        <button className={scope === 'library' ? 'active' : ''} onClick={() => onScope('library')}>Library</button>
        <button className={scope === 'spotify' ? 'active' : ''} onClick={() => onScope('spotify')}>Spotify</button>
      </div>
      <input className="search" type="search" value={query} onChange={(event) => onQuery(event.target.value)} placeholder={`⌕ Search ${scope === 'library' ? 'Library' : 'Spotify'}`} />
      <button className="theme-button" aria-label={`Theme: ${theme}`} title={`Theme: ${theme}`} onClick={onTheme}>{theme === 'system' ? '🖥' : theme === 'dark' ? '☾' : '☀'}</button>
    </div>
  </header>
}

function Sidebar({ state, onSource }: { state: State; onSource: (source: Source) => void }) {
  return <aside className="sidebar">
    <div className="section-label">Library</div>
    {(Object.keys(labels) as Source[]).map((source) => <button key={source} className={`source-row ${state.source === source ? 'active' : ''}`} onClick={() => onSource(source)}>
      <span>{labels[source].icons}</span><span>{labels[source].name}</span><span className="source-count">{state.view?.counts.perSource[source] ?? '—'}</span>
    </button>)}
    <div className="section-label playlists-label">Playlists</div>
    <div className="playlist-placeholder">Recently Added</div>
    <div className="playlist-placeholder">Smart Playlist…</div>
    <div className="overlay-note">🔒 Overlay edits stay local.<br />Never written back to Spotify.</div>
  </aside>
}

function BrowserPane({ state, onSelect }: { state: State; onSelect: (facet: keyof Selection, value?: string) => void }) {
  const sourceLabels = labels[state.source].facets
  const values = [state.view?.facets.cats ?? [], state.view?.facets.arts ?? [], state.view?.facets.albs ?? []]
  const facets: (keyof Selection)[] = ['cat', 'art', 'alb']
  return <div className="browser-pane">
    {facets.map((facet, index) => <FacetColumn key={facet} title={sourceLabels[index]} values={values[index]} selected={state.sel[facet]} onSelect={(value) => onSelect(facet, value)} />)}
  </div>
}

function FacetColumn({ title, values, selected, onSelect }: { title: string; values: string[]; selected?: string; onSelect: (value?: string) => void }) {
  return <div className="facet-column">
    <div className="column-header">{title}</div>
    <div className="facet-list">
      <button className={!selected ? 'active' : ''} onClick={() => onSelect(undefined)}>All ({values.length} {title}s)</button>
      {values.map((value) => <button key={value} className={selected === value ? 'active' : ''} onClick={() => onSelect(value)} title={value}>{value}</button>)}
    </div>
  </div>
}

function RatingStars({ rating, explicit = false, onRate }: { rating: number | null; explicit?: boolean; onRate: (stars: number) => void }) {
  return <span className={`rating-stars ${rating ? explicit ? 'explicit' : 'inherited' : 'empty'}`} aria-label={rating ? `${rating} out of 5 stars` : 'Unrated'}>
    {[1, 2, 3, 4, 5].map((star) => <button key={star} aria-label={`${star} stars`} onClick={(event) => { event.stopPropagation(); onRate(star) }}>{star <= (rating ?? 0) ? '★' : '☆'}</button>)}
  </span>
}

function AlbumRatingStrip({ album, rating, onRate }: { album: string; rating: number | null; onRate: (rating: number | null) => void }) {
  return <div className="album-rating-strip"><strong>{album}</strong><RatingStars rating={rating} explicit onRate={(stars) => onRate(stars === rating ? null : stars)} /><span>· applies to all tracks unless individually overridden</span></div>
}

function TrackList({ tracks, label, selectedId, playing, onSelect, onPlay, onRate, onInfo }: {
  tracks: Track[]; label: (typeof labels)[Source]; selectedId?: number; playing: State['playing']
  onSelect: (id: number) => void; onPlay: (id: number) => void; onRate: (id: number, stars: number) => void; onInfo: (id: number) => void
}) {
  return <div className="track-list">
    <div className="track-row track-header"><span /><span>{label.item[0].toUpperCase() + label.item.slice(1)}</span><span>Time</span><span>{label.facets[1]}</span><span>{label.facets[2]}</span><span>{label.facets[0]}</span><span>Rating</span></div>
    <div className="track-scroll">
      {tracks.map((track) => {
        const isPlaying = playing?.trackId === track.id
        return <div key={track.id} className={`track-row ${selectedId === track.id ? 'selected' : ''}`} onClick={() => onSelect(track.id)} onDoubleClick={() => onPlay(track.id)}>
          <span className="playing-marker">{isPlaying ? playing.isPlaying ? '▶' : '❚❚' : ''}</span>
          <span className="track-name" title={track.name}>{track.name}{selectedId === track.id && <button className="info-button" aria-label={`Get info for ${track.name}`} onClick={(event) => { event.stopPropagation(); onInfo(track.id) }}>ⓘ</button>}</span><span>{formatTime(track.durationSecs)}</span><span title={track.art}>{track.art}</span><span title={track.alb}>{track.alb}</span><span title={track.cat}>{track.overridden ? '● ' : ''}{track.cat}</span>
          <RatingStars rating={track.rating?.stars ?? null} explicit={track.rating?.explicit} onRate={(stars) => onRate(track.id, stars)} />
        </div>
      })}
    </div>
  </div>
}

function GetInfo({ track, onCancel, onSaved, onError }: { track: TrackInfo; onCancel: () => void; onSaved: () => void; onError: (error: string) => void }) {
  const [draft, setDraft] = useState({ name: track.name, art: track.art, alb: track.alb, cat: track.cat })
  const [rating, setRating] = useState(track.rating)
  const dialog = useRef<HTMLDivElement>(null)
  useEffect(() => { dialog.current?.focus() }, [])
  const rate = (stars: number) => setRating((current) => current?.explicit && current.stars === stars
    ? track.inheritedRating === null ? null : { stars: track.inheritedRating, explicit: false }
    : { stars, explicit: true })
  const save = async () => {
    try {
      await invoke('edit_track', { id: track.id, edit: draft })
      if (track.rating?.explicit && (!rating?.explicit || rating.stars !== track.rating.stars)) {
        await invoke('click_track_star', { id: track.id, stars: rating?.explicit ? rating.stars : track.rating.stars })
      } else if (!track.rating?.explicit && rating?.explicit) {
        await invoke('click_track_star', { id: track.id, stars: rating.stars })
      }
      onSaved()
    } catch (error) {
      onError(String(error))
    }
  }
  const field = (key: keyof typeof draft) => ({
    value: draft[key],
    onChange: (event: React.ChangeEvent<HTMLInputElement>) => setDraft({ ...draft, [key]: event.target.value }),
  })
  return <div className="modal-backdrop" role="presentation">
    <div className="get-info" role="dialog" aria-modal="true" aria-labelledby="get-info-title" tabIndex={-1} ref={dialog} onKeyDown={(event) => { if (event.key === 'Escape') onCancel() }}>
      <h2 id="get-info-title">Get Info</h2>
      <label>Spotify ID<input value={track.uri} readOnly /></label>
      <label>Name<input {...field('name')} /></label>
      <label>Artist<input {...field('art')} /></label>
      <label>Album<input {...field('alb')} /></label>
      <label>Genre<input {...field('cat')} list={`genres-${track.id}`} /></label>
      <datalist id={`genres-${track.id}`}>{track.genres.map((genre) => <option key={genre} value={genre} />)}</datalist>
      <div className="genre-hint">normalize freely, e.g. “Operatic Rock” → “Rock”</div>
      <div className="info-rating"><span>Track Rating</span><RatingStars rating={rating?.stars ?? null} explicit={rating?.explicit} onRate={rate} /></div>
      {track.origCat && draft.cat !== track.origCat && <div className="override-banner">Spotify reports this as “{track.origCat}”. Your overlay wins in Retune.</div>}
      <div className="modal-actions"><button onClick={onCancel}>Cancel</button><button className="primary" onClick={() => void save()}>Save Overlay</button></div>
    </div>
  </div>
}

function StatusBar({ view, unit }: { view: BrowseView | null; unit: string }) {
  const total = view?.counts.totalSecs ?? 0
  const hours = Math.floor(total / 3600)
  const minutes = Math.floor((total % 3600) / 60)
  const count = view?.counts.tracks ?? 0
  return <footer className="status-bar"><button aria-label="Add">+</button><span>{count} {count === 1 ? unit : `${unit}s`}, {hours}:{String(minutes).padStart(2, '0')} hours</span><span>{view?.counts.overlayEdits ?? 0} overlay edits</span></footer>
}

export default App
