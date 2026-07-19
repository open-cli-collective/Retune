import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { useCallback, useEffect, useReducer, useRef, useState } from 'react'
import './App.css'

type Source = 'music' | 'podcasts' | 'audiobooks'
type Theme = 'light' | 'dark' | 'system'
type ColumnKey = 'name' | 'time' | 'artist' | 'album' | 'genre' | 'rating'
type Selection = { cat?: string; art?: string; alb?: string }

type Settings = {
  theme: Theme
  zoom: number
  zebra: boolean
  columnOrder: ColumnKey[]
  autoAddSpotifyLibrary: boolean
}

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
  settings: Settings
  settingsHydrated: boolean
  systemDark: boolean
  view: BrowseView | null
  revision: number
  error?: string
  notice?: string
  info?: TrackInfo
  preferences: boolean
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
  | { type: 'hydrateSettings'; settings: Settings }
  | { type: 'settings'; settings: Partial<Settings> }
  | { type: 'systemTheme'; dark: boolean }
  | { type: 'refresh' }
  | { type: 'notice'; notice?: string }
  | { type: 'info'; info?: TrackInfo }
  | { type: 'preferences'; open: boolean }

const defaultSettings: Settings = {
  theme: 'system',
  zoom: 1,
  zebra: true,
  columnOrder: ['name', 'time', 'artist', 'album', 'genre', 'rating'],
  autoAddSpotifyLibrary: false,
}

const initialState: State = {
  source: 'music',
  sel: {},
  query: '',
  scope: 'library',
  playing: null,
  settings: defaultSettings,
  settingsHydrated: false,
  systemDark: false,
  view: null,
  revision: 0,
  preferences: false,
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
    case 'hydrateSettings':
      return { ...state, settings: action.settings, settingsHydrated: true }
    case 'settings':
      return { ...state, settings: { ...state.settings, ...action.settings } }
    case 'systemTheme':
      return { ...state, systemDark: action.dark }
    case 'refresh':
      return { ...state, revision: state.revision + 1 }
    case 'notice':
      return { ...state, notice: action.notice }
    case 'info':
      return { ...state, info: action.info, preferences: false }
    case 'preferences':
      return { ...state, preferences: action.open, info: undefined }
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
  const search = useRef<HTMLInputElement>(null)
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
    invoke<Settings>('get_settings')
      .then((settings) => dispatch({ type: 'hydrateSettings', settings }))
      .catch((error) => dispatch({ type: 'error', error: String(error) }))
    invoke<string | null>('startup_notice')
      .then((notice) => dispatch({ type: 'notice', notice: notice ?? undefined }))
      .catch((error) => dispatch({ type: 'error', error: String(error) }))
  }, [])

  useEffect(() => {
    if (!state.settingsHydrated) return
    invoke('set_settings', { settings: state.settings })
      .catch((error) => dispatch({ type: 'error', error: String(error) }))
  }, [state.settings, state.settingsHydrated])

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
    document.documentElement.dataset.theme = state.settings.theme === 'system'
      ? state.systemDark ? 'dark' : 'light'
      : state.settings.theme
  }, [state.settings.theme, state.systemDark])

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
  const step = useCallback((direction: number) => {
    if (!tracks.length) return
    const index = tracks.findIndex((track) => track.id === state.playing?.trackId)
    const next = tracks[(index < 0 ? 0 : index + direction + tracks.length) % tracks.length]
    dispatch({ type: 'step', id: next.id })
  }, [state.playing?.trackId, tracks])
  const setZoom = (zoom: number) => dispatch({
    type: 'settings',
    settings: { zoom: Math.min(1.8, Math.max(0.7, Math.round(zoom * 10) / 10)) },
  })
  const cycleTheme = () => dispatch({
    type: 'settings',
    settings: { theme: state.settings.theme === 'system' ? 'light' : state.settings.theme === 'light' ? 'dark' : 'system' },
  })
  const playingTrack = tracks.find((track) => track.id === state.playing?.trackId)

  useEffect(() => {
    const viewActions = listen<string>('view-action', ({ payload }) => {
      if (payload === 'zoom_in') setZoom(state.settings.zoom + 0.1)
      else if (payload === 'zoom_out') setZoom(state.settings.zoom - 0.1)
      else if (payload === 'actual_size') setZoom(1)
      else if (payload === 'toggle_zebra') dispatch({ type: 'settings', settings: { zebra: !state.settings.zebra } })
      else if (payload.startsWith('theme_')) dispatch({ type: 'settings', settings: { theme: payload.slice(6) as Theme } })
    })
    const playerActions = listen<string>('player-action', ({ payload }) => {
      if (payload === 'play_pause') dispatch({ type: 'togglePlay' })
      else step(payload === 'previous' ? -1 : 1)
    })
    const preferences = listen('open-preferences', () => dispatch({ type: 'preferences', open: true }))
    return () => {
      void viewActions.then((stop) => stop())
      void playerActions.then((stop) => stop())
      void preferences.then((stop) => stop())
    }
  }, [state.settings, step])

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const modalOpen = Boolean(state.info || state.preferences)
      if (event.key === 'Escape' && modalOpen) {
        event.preventDefault()
        if (state.info) dispatch({ type: 'info' })
        else dispatch({ type: 'preferences', open: false })
        return
      }
      const target = event.target as HTMLElement | null
      if (modalOpen || target?.closest('input, textarea, select')) return
      const command = event.metaKey || event.ctrlKey
      if (command && ['=', '+', '-', '0'].includes(event.key)) {
        event.preventDefault()
        setZoom(event.key === '0' ? 1 : state.settings.zoom + (event.key === '-' ? -0.1 : 0.1))
      } else if (command && event.key.toLowerCase() === 'i') {
        event.preventDefault()
        openInfo(state.selectedTrackId)
      } else if (command && event.key.toLowerCase() === 'l') {
        event.preventDefault()
        dispatch({ type: 'scope', scope: 'library' })
        window.requestAnimationFrame(() => search.current?.focus())
      } else if (command && event.key === ',') {
        event.preventDefault()
        dispatch({ type: 'preferences', open: true })
      } else if (!command && event.key === ' ') {
        event.preventDefault()
        dispatch({ type: 'togglePlay' })
      } else if (!command && event.key === 'ArrowLeft') {
        event.preventDefault()
        step(-1)
      } else if (!command && event.key === 'ArrowRight') {
        event.preventDefault()
        step(1)
      }
    }
    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [state.info, state.preferences, state.selectedTrackId, state.settings.zoom, step])

  useEffect(() => {
    const onWheel = (event: WheelEvent) => {
      if (!(event.metaKey || event.ctrlKey) || state.info || state.preferences) return
      event.preventDefault()
      setZoom(state.settings.zoom + (event.deltaY < 0 ? 0.1 : -0.1))
    }
    window.addEventListener('wheel', onWheel, { passive: false })
    return () => window.removeEventListener('wheel', onWheel)
  }, [state.info, state.preferences, state.settings.zoom])

  return (
    <main className={`app-shell ${state.settings.zebra ? 'zebra' : ''}`} style={{ zoom: state.settings.zoom }}>
      <TransportBar
        playing={state.playing}
        track={playingTrack}
        query={state.query}
        scope={state.scope}
        theme={state.settings.theme}
        searchRef={search}
        onQuery={(query) => dispatch({ type: 'query', query })}
        onScope={(scope) => dispatch({ type: 'scope', scope })}
        onPlay={() => dispatch({ type: 'togglePlay' })}
        onPrev={() => step(-1)}
        onNext={() => step(1)}
        onTheme={cycleTheme}
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
              columnOrder={state.settings.columnOrder}
              onSelect={(id) => dispatch({ type: 'selectTrack', id })}
              onPlay={(id) => dispatch({ type: 'play', id })}
              onRate={(id, stars) => mutate('click_track_star', { id, stars })}
              onInfo={openInfo}
              onReorder={(columnOrder) => dispatch({ type: 'settings', settings: { columnOrder } })}
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
      {state.preferences && <Preferences settings={state.settings} onCancel={() => dispatch({ type: 'preferences', open: false })} onSave={(autoAddSpotifyLibrary) => {
        dispatch({ type: 'settings', settings: { autoAddSpotifyLibrary } })
        dispatch({ type: 'preferences', open: false })
      }} />}
    </main>
  )
}

function TransportBar({ playing, track, query, scope, theme, searchRef, onQuery, onScope, onPlay, onPrev, onNext, onTheme }: {
  playing: State['playing']; track?: Track; query: string; scope: State['scope']; theme: Theme
  searchRef: React.RefObject<HTMLInputElement | null>
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
      <input ref={searchRef} className="search" type="search" value={query} onChange={(event) => onQuery(event.target.value)} placeholder={`⌕ Search ${scope === 'library' ? 'Library' : 'Spotify'}`} />
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

function TrackList({ tracks, label, selectedId, playing, columnOrder, onSelect, onPlay, onRate, onInfo, onReorder }: {
  tracks: Track[]; label: (typeof labels)[Source]; selectedId?: number; playing: State['playing']
  columnOrder: ColumnKey[]; onSelect: (id: number) => void; onPlay: (id: number) => void
  onRate: (id: number, stars: number) => void; onInfo: (id: number) => void; onReorder: (order: ColumnKey[]) => void
}) {
  const [dragging, setDragging] = useState<ColumnKey>()
  const headings: Record<ColumnKey, string> = {
    name: label.item[0].toUpperCase() + label.item.slice(1),
    time: 'Time',
    artist: label.facets[1],
    album: label.facets[2],
    genre: label.facets[0],
    rating: 'Rating',
  }
  const widths: Record<ColumnKey, string> = { name: 'minmax(160px, 1.6fr)', time: '52px', artist: '1.1fr', album: '1.1fr', genre: '.9fr', rating: '84px' }
  const columns = `22px ${columnOrder.map((column) => widths[column]).join(' ')}`
  const drop = (target: ColumnKey) => {
    if (!dragging || dragging === target) return
    const next = columnOrder.filter((column) => column !== dragging)
    next.splice(next.indexOf(target), 0, dragging)
    onReorder(next)
  }
  const cell = (track: Track, column: ColumnKey) => {
    if (column === 'name') return <span key={column} className="track-name" title={track.name}>{track.name}{selectedId === track.id && <button className="info-button" aria-label={`Get info for ${track.name}`} onClick={(event) => { event.stopPropagation(); onInfo(track.id) }}>ⓘ</button>}</span>
    if (column === 'time') return <span key={column}>{formatTime(track.durationSecs)}</span>
    if (column === 'artist') return <span key={column} title={track.art}>{track.art}</span>
    if (column === 'album') return <span key={column} title={track.alb}>{track.alb}</span>
    if (column === 'genre') return <span key={column} title={track.cat}>{track.overridden ? '● ' : ''}{track.cat}</span>
    return <RatingStars key={column} rating={track.rating?.stars ?? null} explicit={track.rating?.explicit} onRate={(stars) => onRate(track.id, stars)} />
  }
  return <div className="track-list">
    <div className="track-row track-header" style={{ gridTemplateColumns: columns }}><span />{columnOrder.map((column) => <span key={column} draggable className={dragging === column ? 'dragging' : ''} onDragStart={(event) => {
      setDragging(column)
      event.dataTransfer.effectAllowed = 'move'
    }} onDragEnd={() => setDragging(undefined)} onDragOver={(event) => event.preventDefault()} onDrop={() => drop(column)}>{headings[column]}</span>)}</div>
    <div className="track-scroll">
      {tracks.map((track) => {
        const isPlaying = playing?.trackId === track.id
        return <div key={track.id} className={`track-row ${selectedId === track.id ? 'selected' : ''}`} style={{ gridTemplateColumns: columns }} onClick={() => onSelect(track.id)} onDoubleClick={() => onPlay(track.id)}>
          <span className="playing-marker">{isPlaying ? playing.isPlaying ? '▶' : '❚❚' : ''}</span>
          {columnOrder.map((column) => cell(track, column))}
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
    <div className="get-info" role="dialog" aria-modal="true" aria-labelledby="get-info-title" tabIndex={-1} ref={dialog}>
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

function Preferences({ settings, onCancel, onSave }: { settings: Settings; onCancel: () => void; onSave: (autoAdd: boolean) => void }) {
  const [autoAdd, setAutoAdd] = useState(settings.autoAddSpotifyLibrary)
  const dialog = useRef<HTMLDivElement>(null)
  useEffect(() => { dialog.current?.focus() }, [])
  return <div className="modal-backdrop" role="presentation">
    <div className="get-info preferences" role="dialog" aria-modal="true" aria-labelledby="preferences-title" tabIndex={-1} ref={dialog}>
      <h2 id="preferences-title">Preferences</h2>
      <fieldset>
        <legend>Library</legend>
        <label className="checkbox"><input type="checkbox" checked={autoAdd} onChange={(event) => setAutoAdd(event.target.checked)} />Automatically add my entire Spotify library</label>
        <p>Takes effect when Spotify is connected.</p>
      </fieldset>
      <div className="modal-actions"><button onClick={onCancel}>Cancel</button><button className="primary" onClick={() => onSave(autoAdd)}>Save</button></div>
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
