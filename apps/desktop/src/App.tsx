import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { useCallback, useEffect, useMemo, useReducer, useRef, useState } from 'react'
import './App.css'

type Source = 'music' | 'podcasts' | 'audiobooks'
type Theme = 'light' | 'dark' | 'system'
type PlaybackBackend = 'connect' | 'local'
type RepeatMode = 'off' | 'all' | 'one'
type ColumnKey = 'track' | 'name' | 'time' | 'artist' | 'album' | 'genre' | 'rating'
type Selection = { cat?: string; art?: string; alb?: string }
type ActivePane = 'track' | keyof Selection

type Settings = {
  theme: Theme
  zoom: number
  zebra: boolean
  columnOrder: ColumnKey[]
  hiddenColumns: ColumnKey[]
  autoAddSpotifyLibrary: boolean
  autoConnect: boolean
  spotifyClientId: string
  spotifySyncCompleted: boolean
  playbackBackend: PlaybackBackend
  repeat: RepeatMode
  volume: number
}

type ConnectionState = { connected: boolean }
type SpotifyResults = {
  artists: { name: string; uri: string }[]
  albums: { name: string; artist: string; uri: string; trackCount: number | null }[]
}

type Track = {
  id: number
  uri: string
  name: string
  art: string
  alb: string
  cat: string
  trackNo: number | null
  durationSecs: number
  overridden: boolean
  rating: { stars: number; explicit: boolean } | null
}

type PlayerState = {
  trackId: number | null
  elapsed: number
  isPlaying: boolean
  external: boolean
  name: string | null
  art: string | null
  alb: string | null
  durationSecs: number | null
  volumeSupported: boolean
}

// `simulated` marks a local prototype session (disconnected, or fixture tracks
// whose URIs must never reach the real Spotify API).
type Playing = PlayerState & { queue: readonly Track[]; simulated?: boolean }

type BrowseView = {
  facets: { cats: string[]; arts: string[]; albs: string[] }
  tracks: Track[]
  albumRating: number | null
  albumRatingArtist: string | null
  albumRatingAmbiguous: boolean
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

type InfoDialog =
  | { kind: 'single'; track: TrackInfo }
  | { kind: 'multiple'; tracks: Track[] }

const emptyTracks: Track[] = []

type State = {
  source: Source
  sel: Selection
  query: string
  scope: 'library' | 'spotify'
  selectedTrackIds: Set<number>
  selectionAnchor?: number
  playing: Playing | null
  settings: Settings
  settingsHydrated: boolean
  systemDark: boolean
  view: BrowseView | null
  revision: number
  error?: string
  notice?: string
  info?: InfoDialog
  preferences: boolean
  connected: boolean
  spotifyResults: SpotifyResults | null
  spotifySearching: boolean
  syncPhase?: string
}

type Action =
  | { type: 'view'; view: BrowseView }
  | { type: 'error'; error: string }
  | { type: 'source'; source: Source }
  | { type: 'select'; facet: keyof Selection; value?: string }
  | { type: 'query'; query: string }
  | { type: 'scope'; scope: State['scope'] }
  | { type: 'selectTrack'; id: number }
  | { type: 'selection'; ids: Set<number>; anchor?: number }
  | { type: 'play'; id: number; queue: readonly Track[] }
  | { type: 'togglePlay' }
  | { type: 'step'; id: number }
  | { type: 'tick'; duration: number; nextId: number }
  | { type: 'seek'; elapsed: number }
  | { type: 'playerState'; player: PlayerState; queue: readonly Track[] }
  | { type: 'hydrateSettings'; settings: Settings }
  | { type: 'settings'; settings: Partial<Settings> }
  | { type: 'systemTheme'; dark: boolean }
  | { type: 'refresh' }
  | { type: 'notice'; notice?: string }
  | { type: 'info'; info?: InfoDialog }
  | { type: 'preferences'; open: boolean }
  | { type: 'connection'; connected: boolean }
  | { type: 'spotifyResults'; results: SpotifyResults | null }
  | { type: 'spotifySearching'; searching: boolean }
  | { type: 'syncPhase'; phase?: string }

const defaultSettings: Settings = {
  theme: 'system',
  zoom: 1,
  zebra: true,
  columnOrder: ['track', 'name', 'time', 'artist', 'album', 'genre', 'rating'],
  hiddenColumns: [],
  autoAddSpotifyLibrary: true,
  autoConnect: true,
  spotifyClientId: '',
  spotifySyncCompleted: false,
  playbackBackend: 'connect',
  repeat: 'off',
  volume: 62,
}

const initialState: State = {
  source: 'music',
  sel: {},
  query: '',
  scope: 'library',
  selectedTrackIds: new Set(),
  playing: null,
  settings: defaultSettings,
  settingsHydrated: false,
  systemDark: false,
  view: null,
  revision: 0,
  preferences: false,
  connected: false,
  spotifyResults: null,
  spotifySearching: false,
}

function reducer(state: State, action: Action): State {
  switch (action.type) {
    case 'view':
      return { ...state, view: action.view, error: undefined }
    case 'error':
      return { ...state, error: action.error }
    case 'source':
      return { ...state, source: action.source, sel: {}, query: '', selectedTrackIds: new Set(), selectionAnchor: undefined }
    case 'select': {
      const sel = action.facet === 'cat'
        ? { cat: action.value }
        : action.facet === 'art'
          ? { cat: state.sel.cat, art: action.value }
          : { ...state.sel, alb: action.value }
      return { ...state, sel, selectedTrackIds: new Set(), selectionAnchor: undefined }
    }
    case 'query':
      return { ...state, query: action.query, selectedTrackIds: new Set(), selectionAnchor: undefined }
    case 'scope':
      return { ...state, scope: action.scope }
    case 'selectTrack':
      return { ...state, selectedTrackIds: new Set([action.id]), selectionAnchor: action.id }
    case 'selection':
      return { ...state, selectedTrackIds: action.ids, selectionAnchor: action.anchor }
    case 'play':
      return {
        ...state,
        selectedTrackIds: new Set([action.id]),
        selectionAnchor: action.id,
        playing: {
          trackId: action.id, elapsed: 0, isPlaying: true, queue: action.queue,
          external: false, name: null, art: null, alb: null, durationSecs: null,
          volumeSupported: false, simulated: true,
        },
      }
    case 'togglePlay':
      return state.playing
        ? { ...state, playing: { ...state.playing, isPlaying: !state.playing.isPlaying } }
        : state
    case 'step':
      return state.playing
        ? { ...state, playing: { ...state.playing, trackId: action.id, elapsed: 0, isPlaying: true } }
        : state
    case 'tick':
      if (!state.playing?.isPlaying) return state
      return state.playing.elapsed + 1 >= action.duration
        ? { ...state, playing: { ...state.playing, trackId: action.nextId, elapsed: 0, isPlaying: true } }
        : { ...state, playing: { ...state.playing, elapsed: state.playing.elapsed + 1 } }
    case 'playerState':
      return action.player.trackId === null && !action.player.name
        ? { ...state, playing: null }
        : {
            ...state,
            playing: { ...action.player, queue: action.player.external ? emptyTracks : action.queue },
          }
    case 'seek':
      return state.playing
        ? { ...state, playing: { ...state.playing, elapsed: action.elapsed } }
        : state
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
    case 'connection':
      return { ...state, connected: action.connected }
    case 'spotifyResults':
      return { ...state, spotifyResults: action.results, spotifySearching: false }
    case 'spotifySearching':
      return { ...state, spotifySearching: action.searching }
    case 'syncPhase':
      return { ...state, syncPhase: action.phase }
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

function usePlayer(connected: boolean, playing: Playing | null, dispatch: React.Dispatch<Action>) {
  const queue = useRef<readonly Track[]>(emptyTracks)
  const pendingPlay = useRef<{ id: number; tracks: readonly Track[] } | null>(null)
  const playingRef = useRef(playing)
  const volumeTimer = useRef<number>(undefined)
  playingRef.current = playing

  useEffect(() => {
    const playerState = listen<PlayerState>('player-state', ({ payload }) => {
      dispatch({ type: 'playerState', player: payload, queue: queue.current })
    })
    return () => { void playerState.then((stop) => stop()) }
  }, [dispatch])

  const run = useCallback((command: string, args?: Record<string, unknown>) => {
    invoke(command, args).catch((error) => dispatch({ type: 'error', error: String(error) }))
  }, [dispatch])

  const start = useCallback((id: number, tracks: readonly Track[]) => {
    const target = tracks.find((track) => track.id === id)
    if (!target?.uri.startsWith('spotify:')) {
      dispatch({ type: 'play', id, queue: tracks })
      return
    }
    if (!connected) {
      // Kick off the OAuth flow instead of erroring; the pending play fires
      // once connection-changed reports connected.
      pendingPlay.current = { id, tracks }
      run('connect_spotify')
      return
    }
    queue.current = tracks
    run('play_tracks', { snapshot: tracks, startIndex: tracks.findIndex((track) => track.id === id) })
  }, [connected, dispatch, run])

  useEffect(() => {
    if (!connected || !pendingPlay.current) return
    const { id, tracks } = pendingPlay.current
    pendingPlay.current = null
    queue.current = tracks
    run('play_tracks', { snapshot: tracks, startIndex: tracks.findIndex((track) => track.id === id) })
  }, [connected, run])

  const toggle = useCallback(() => {
    if (connected && !playingRef.current?.simulated) {
      if (playingRef.current && !playingRef.current.external) run('player_toggle')
    }
    else dispatch({ type: 'togglePlay' })
  }, [connected, dispatch, run])

  const step = useCallback((direction: number) => {
    if (connected && !playingRef.current?.simulated) {
      if (playingRef.current && !playingRef.current.external) run(direction < 0 ? 'player_prev' : 'player_next')
      return
    }
    const current = playingRef.current
    if (!current?.queue.length || current.trackId === null) return
    const index = current.queue.findIndex((track) => track.id === current.trackId)
    const next = current.queue[(index + direction + current.queue.length) % current.queue.length]
    dispatch({ type: 'step', id: next.id })
  }, [connected, dispatch, run])

  const setVolume = useCallback((volume: number) => {
    if (!connected || playingRef.current?.simulated) return
    window.clearTimeout(volumeTimer.current)
    volumeTimer.current = window.setTimeout(() => run('player_set_volume', { volume }), 150)
  }, [connected, run])

  const seek = useCallback((seconds: number) => {
    if (connected && !playingRef.current?.simulated) {
      if (playingRef.current && !playingRef.current.external) run('player_seek', { seconds })
      return
    }
    dispatch({ type: 'seek', elapsed: seconds })
  }, [connected, dispatch, run])

  useEffect(() => () => window.clearTimeout(volumeTimer.current), [])

  return useMemo(() => ({ start, toggle, step, setVolume, seek }), [seek, setVolume, start, step, toggle])
}

function App() {
  const [state, dispatch] = useReducer(reducer, initialState)
  const [activePane, setActivePane] = useState<ActivePane>('track')
  const search = useRef<HTMLInputElement>(null)
  const typeahead = useRef({ buffer: '', timer: 0 })
  const typeaheadExpires = useRef(0)
  const view = state.view
  const tracks = view?.tracks ?? emptyTracks
  const selectedTracks = tracks.filter((track) => state.selectedTrackIds.has(track.id))
  const tracklistVisible = state.scope !== 'spotify' || !state.query.trim()
  const playbackTracks = state.playing?.queue ?? emptyTracks
  const player = usePlayer(state.connected, state.playing, dispatch)
  const cycleRepeat = () => {
    const repeat: RepeatMode = state.settings.repeat === 'off' ? 'all' : state.settings.repeat === 'all' ? 'one' : 'off'
    invoke('set_repeat', { mode: repeat })
      .then(() => dispatch({ type: 'settings', settings: { repeat } }))
      .catch((error) => dispatch({ type: 'error', error: String(error) }))
  }
  const openInfo = (id?: number) => {
    if (selectedTracks.length > 1) {
      dispatch({ type: 'info', info: { kind: 'multiple', tracks: selectedTracks } })
      return
    }
    const target = id ?? selectedTracks[0]?.id
    if (target === undefined) return
    invoke<TrackInfo>('get_track', { id: target })
      .then((track) => dispatch({ type: 'info', info: { kind: 'single', track } }))
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
    invoke<ConnectionState>('connection_state')
      .then(({ connected }) => dispatch({ type: 'connection', connected }))
      .catch((error) => dispatch({ type: 'error', error: String(error) }))
  }, [])

  useEffect(() => {
    if (!state.settingsHydrated) return
    invoke('set_settings', { settings: state.settings })
      .catch((error) => dispatch({ type: 'error', error: String(error) }))
  }, [
    state.settings.theme,
    state.settings.zoom,
    state.settings.zebra,
    state.settings.columnOrder,
    state.settings.hiddenColumns,
    state.settings.autoAddSpotifyLibrary,
    state.settings.autoConnect,
    state.settings.spotifyClientId,
    state.settings.spotifySyncCompleted,
    state.settings.playbackBackend,
    state.settings.volume,
    state.settingsHydrated,
  ])

  useEffect(() => {
    const unlisten = listen('get-info', () => openInfo())
    return () => { void unlisten.then((stop) => stop()) }
  }, [state.selectedTrackIds, tracks])

  useEffect(() => {
    const changed = listen('library-changed', () => dispatch({ type: 'refresh' }))
    const failed = listen<string>('operation-error', ({ payload }) => dispatch({ type: 'error', error: payload }))
    const connection = listen<ConnectionState>('connection-changed', ({ payload }) => dispatch({ type: 'connection', connected: payload.connected }))
    const settings = listen<Settings>('settings-changed', ({ payload }) => dispatch({ type: 'hydrateSettings', settings: payload }))
    const progress = listen<string>('sync-progress', ({ payload }) => dispatch({ type: 'syncPhase', phase: payload || undefined }))
    return () => {
      void changed.then((stop) => stop())
      void failed.then((stop) => stop())
      void connection.then((stop) => stop())
      void settings.then((stop) => stop())
      void progress.then((stop) => stop())
    }
  }, [])

  useEffect(() => {
    const query = state.query.trim()
    if (state.scope !== 'spotify' || !query || !state.connected) {
      dispatch({ type: 'spotifyResults', results: null })
      return
    }
    dispatch({ type: 'spotifySearching', searching: true })
    let active = true
    const timer = window.setTimeout(() => {
      invoke<SpotifyResults>('spotify_search', { query })
        .then((results) => active && dispatch({ type: 'spotifyResults', results }))
        .catch((error) => {
          if (!active) return
          dispatch({ type: 'spotifySearching', searching: false })
          dispatch({ type: 'error', error: String(error) })
        })
    }, 300)
    return () => {
      active = false
      window.clearTimeout(timer)
    }
  }, [state.scope, state.query, state.connected])

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
    const title = state.source === 'music' ? 'Retune — Library' : `Retune — ${labels[state.source].name}`
    getCurrentWindow().setTitle(title).catch((error) => dispatch({ type: 'error', error: String(error) }))
  }, [state.source])

  useEffect(() => {
    if (state.connected && !state.playing?.simulated) return
    if (!state.playing?.isPlaying) return
    const currentIndex = playbackTracks.findIndex((track) => track.id === state.playing?.trackId)
    const current = playbackTracks[currentIndex]
    if (!current) return
    const next = playbackTracks[(currentIndex + 1) % playbackTracks.length]
    const timer = window.setInterval(() => {
      dispatch({ type: 'tick', duration: current.durationSecs, nextId: next.id })
    }, 1000)
    return () => window.clearInterval(timer)
  }, [state.connected, state.playing?.trackId, state.playing?.isPlaying, playbackTracks])

  const mutate = (command: string, args: Record<string, unknown>) => {
    invoke(command, args)
      .then(() => dispatch({ type: 'refresh' }))
      .catch((error) => dispatch({ type: 'error', error: String(error) }))
  }
  const setZoom = (zoom: number) => dispatch({
    type: 'settings',
    settings: { zoom: Math.min(1.8, Math.max(0.7, Math.round(zoom * 10) / 10)) },
  })
  const cycleTheme = () => dispatch({
    type: 'settings',
    settings: { theme: state.settings.theme === 'system' ? 'light' : state.settings.theme === 'light' ? 'dark' : 'system' },
  })
  const playingTrack = playbackTracks.find((track) => track.id === state.playing?.trackId)

  useEffect(() => {
    const viewActions = listen<string>('view-action', ({ payload }) => {
      if (payload === 'zoom_in') setZoom(state.settings.zoom + 0.1)
      else if (payload === 'zoom_out') setZoom(state.settings.zoom - 0.1)
      else if (payload === 'actual_size') setZoom(1)
      else if (payload === 'toggle_zebra') dispatch({ type: 'settings', settings: { zebra: !state.settings.zebra } })
    })
    const playerActions = listen<string>('player-action', ({ payload }) => {
      if (payload === 'play_pause') player.toggle()
      else player.step(payload === 'previous' ? -1 : 1)
    })
    const preferences = listen('open-preferences', () => dispatch({ type: 'preferences', open: true }))
    return () => {
      void viewActions.then((stop) => stop())
      void playerActions.then((stop) => stop())
      void preferences.then((stop) => stop())
    }
  }, [state.settings, player])

  useEffect(() => {
    if (typeahead.current.buffer) {
      const remaining = typeaheadExpires.current - Date.now()
      if (remaining > 0) typeahead.current.timer = window.setTimeout(() => { typeahead.current.buffer = '' }, remaining)
      else typeahead.current.buffer = ''
    }
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
      } else if (command && event.key.toLowerCase() === 'a' && activePane === 'track' && tracklistVisible) {
        event.preventDefault()
        const anchor = tracks.some((track) => track.id === state.selectionAnchor)
          ? state.selectionAnchor
          : tracks[0]?.id
        dispatch({ type: 'selection', ids: new Set(tracks.map((track) => track.id)), anchor })
      } else if (command && event.key.toLowerCase() === 'i') {
        event.preventDefault()
        openInfo()
      } else if (command && event.key.toLowerCase() === 'l') {
        event.preventDefault()
        dispatch({ type: 'scope', scope: 'library' })
        window.requestAnimationFrame(() => search.current?.focus())
      } else if (command && event.key === ',') {
        event.preventDefault()
        dispatch({ type: 'preferences', open: true })
      } else if (!command && event.key === ' ' && !typeahead.current.buffer) {
        event.preventDefault()
        player.toggle()
      } else if (!command && event.key.length === 1) {
        event.preventDefault()
        typeahead.current.buffer += event.key
        window.clearTimeout(typeahead.current.timer)
        typeaheadExpires.current = Date.now() + 1000
        typeahead.current.timer = window.setTimeout(() => { typeahead.current.buffer = '' }, 1000)
        const prefix = typeahead.current.buffer.toLocaleLowerCase()
        if (activePane === 'track') {
          const track = tracks.find((track) => track.name.toLocaleLowerCase().startsWith(prefix))
          if (!track) return
          dispatch({ type: 'selectTrack', id: track.id })
          window.requestAnimationFrame(() => document.querySelector<HTMLElement>(`[data-track-id="${track.id}"]`)?.scrollIntoView({ block: 'nearest' }))
        } else {
          const facetValues = state.view?.facets[activePane === 'cat' ? 'cats' : activePane === 'art' ? 'arts' : 'albs'] ?? []
          const index = facetValues.findIndex((value) => value.toLocaleLowerCase().startsWith(prefix))
          if (index < 0) return
          dispatch({ type: 'select', facet: activePane, value: facetValues[index] })
          window.requestAnimationFrame(() => document.querySelector<HTMLElement>(`[data-facet="${activePane}"] [data-row-index="${index + 1}"]`)?.scrollIntoView({ block: 'nearest' }))
        }
      } else if (!command && event.key === 'ArrowLeft') {
        event.preventDefault()
        player.step(-1)
      } else if (!command && event.key === 'ArrowRight') {
        event.preventDefault()
        player.step(1)
      } else if (!command && (event.key === 'ArrowUp' || event.key === 'ArrowDown')) {
        event.preventDefault()
        const direction = event.key === 'ArrowUp' ? -1 : 1
        if (activePane === 'track') {
          if (!tracks.length) return
          const current = tracks.findIndex((track) => track.id === state.selectionAnchor)
          const index = current < 0 ? 0 : Math.max(0, Math.min(tracks.length - 1, current + direction))
          dispatch({ type: 'selectTrack', id: tracks[index].id })
          window.requestAnimationFrame(() => document.querySelector<HTMLElement>(`[data-track-id="${tracks[index].id}"]`)?.scrollIntoView({ block: 'nearest' }))
        } else {
          const facetValues = state.view?.facets[activePane === 'cat' ? 'cats' : activePane === 'art' ? 'arts' : 'albs'] ?? []
          const values: (string | undefined)[] = [undefined, ...facetValues]
          const current = values.indexOf(state.sel[activePane])
          const index = Math.max(0, Math.min(values.length - 1, current + direction))
          dispatch({ type: 'select', facet: activePane, value: values[index] })
          window.requestAnimationFrame(() => document.querySelector<HTMLElement>(`[data-facet="${activePane}"] [data-row-index="${index}"]`)?.scrollIntoView({ block: 'nearest' }))
        }
      }
    }
    window.addEventListener('keydown', onKeyDown)
    return () => {
      window.clearTimeout(typeahead.current.timer)
      window.removeEventListener('keydown', onKeyDown)
    }
  }, [activePane, state.info, state.preferences, state.sel, state.selectedTrackIds, state.selectionAnchor, state.settings.zoom, state.view, tracks, tracklistVisible, player])

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
        connected={state.connected}
        volume={state.settings.volume}
        repeat={state.settings.repeat}
        searchRef={search}
        onQuery={(query) => dispatch({ type: 'query', query })}
        onScope={(scope) => dispatch({ type: 'scope', scope })}
        onPlay={player.toggle}
        onPrev={() => player.step(-1)}
        onNext={() => player.step(1)}
        onRepeat={cycleRepeat}
        onVolume={(volume) => { dispatch({ type: 'settings', settings: { volume } }); player.setVolume(volume) }}
        onSeek={player.seek}
        onTheme={cycleTheme}
      />
      <div className="body-grid">
        <Sidebar state={state} onSource={(source) => dispatch({ type: 'source', source })} />
        <section className="content">
          <BrowserPane state={state} onActivate={setActivePane} onSelect={(facet, value) => dispatch({ type: 'select', facet, value })} />
          {state.sel.alb && view && !view.albumRatingAmbiguous && view.albumRatingArtist !== null && (
            <AlbumRatingStrip
              album={state.sel.alb}
              rating={view?.albumRating ?? null}
              onRate={(stars) => mutate('set_album_rating', {
                source: state.source,
                art: view.albumRatingArtist,
                alb: state.sel.alb,
                stars,
              })}
            />
          )}
          {state.notice && <div className="startup-notice"><span>{state.notice}</span><button aria-label="Dismiss notice" onClick={() => dispatch({ type: 'notice' })}>×</button></div>}
          {state.scope === 'spotify' && state.query.trim() ? (
            state.connected ? <SpotifySearch
              searching={state.spotifySearching}
              results={state.spotifyResults}
              onArtist={(artist) => {
                dispatch({ type: 'spotifySearching', searching: true })
                invoke<SpotifyResults['albums']>('spotify_artist_albums', { artistId: artist.uri })
                  .then((albums) => dispatch({
                    type: 'spotifyResults',
                    results: { artists: state.spotifyResults?.artists ?? [artist], albums },
                  }))
                  .catch((error) => dispatch({ type: 'error', error: String(error) }))
              }}
              onAdd={(album) => invoke('add_spotify_album', album)
                .catch((error) => dispatch({ type: 'error', error: String(error) }))}
            /> : <div className="spotify-stub"><span>Connect to Spotify to search artists and albums.</span><button onClick={() => invoke('connect_spotify').catch((error) => dispatch({ type: 'error', error: String(error) }))}>Connect to Spotify</button></div>
          ) : (
            <TrackList
              tracks={tracks}
              label={labels[state.source]}
              selectedIds={state.selectedTrackIds}
              playing={state.playing}
              columnOrder={state.settings.columnOrder}
              hiddenColumns={state.settings.hiddenColumns}
              onActivate={() => setActivePane('track')}
              onSelect={(id, event) => {
                if (event.shiftKey && state.selectionAnchor !== undefined) {
                  const anchor = tracks.findIndex((track) => track.id === state.selectionAnchor)
                  const row = tracks.findIndex((track) => track.id === id)
                  if (anchor >= 0 && row >= 0) {
                    dispatch({
                      type: 'selection',
                      ids: new Set(tracks.slice(Math.min(anchor, row), Math.max(anchor, row) + 1).map((track) => track.id)),
                      anchor: state.selectionAnchor,
                    })
                    return
                  }
                }
                if (event.metaKey || event.ctrlKey) {
                  const ids = new Set(state.selectedTrackIds)
                  if (!ids.delete(id)) ids.add(id)
                  dispatch({ type: 'selection', ids, anchor: id })
                } else {
                  dispatch({ type: 'selectTrack', id })
                }
              }}
              onPlay={(id) => player.start(id, tracks)}
              onRate={(id, stars) => mutate('click_track_star', { id, stars })}
              onInfo={openInfo}
              onReorder={(columnOrder) => dispatch({ type: 'settings', settings: { columnOrder } })}
              onHiddenColumns={(hiddenColumns) => dispatch({ type: 'settings', settings: { hiddenColumns } })}
            />
          )}
          {state.error && <div className="error-banner">{state.error}</div>}
          <StatusBar view={view} unit={labels[state.source].item} syncPhase={state.syncPhase} />
        </section>
      </div>
      {state.info?.kind === 'single' && <GetInfo key={state.info.track.id} track={state.info.track} onCancel={() => dispatch({ type: 'info' })} onSaved={() => {
        dispatch({ type: 'info' })
        dispatch({ type: 'refresh' })
      }} onError={(error) => dispatch({ type: 'error', error })} />}
      {state.info?.kind === 'multiple' && <MultipleItemInformation tracks={state.info.tracks} onCancel={() => dispatch({ type: 'info' })} onSaved={() => dispatch({ type: 'info' })} onError={(error) => dispatch({ type: 'error', error })} />}
      {state.preferences && <Preferences settings={state.settings} onCancel={() => dispatch({ type: 'preferences', open: false })} onSave={(theme, autoAddSpotifyLibrary, autoConnect, spotifyClientId, playbackBackend) => {
        dispatch({ type: 'settings', settings: { theme, autoAddSpotifyLibrary, autoConnect, spotifyClientId, playbackBackend } })
        dispatch({ type: 'preferences', open: false })
      }} />}
    </main>
  )
}

function Marquee({ text, strong }: { text: string; strong?: boolean }) {
  const outer = useRef<HTMLDivElement>(null)
  const inner = useRef<HTMLSpanElement>(null)
  const [distance, setDistance] = useState(0)
  useEffect(() => {
    const measure = () => setDistance(Math.max(0, (inner.current?.scrollWidth ?? 0) - (outer.current?.clientWidth ?? 0)))
    measure()
    window.addEventListener('resize', measure)
    return () => window.removeEventListener('resize', measure)
  }, [text])
  return <div ref={outer} className="lcd-line">
    <span
      ref={inner}
      className={`marquee${distance > 0 ? ' scrolling' : ''}${strong ? ' strong' : ''}`}
      style={distance > 0 ? { '--marquee-distance': `-${distance}px`, animationDuration: `${Math.max(8, Math.round(distance / 12))}s` } as React.CSSProperties : undefined}
    >{text}</span>
  </div>
}

function TransportBar({ playing, track, query, scope, theme, connected, volume, repeat, searchRef, onQuery, onScope, onPlay, onPrev, onNext, onRepeat, onVolume, onSeek, onTheme }: {
  playing: State['playing']; track?: Track; query: string; scope: State['scope']; theme: Theme
  connected: boolean; volume: number; repeat: RepeatMode
  searchRef: React.RefObject<HTMLInputElement | null>
  onQuery: (query: string) => void; onScope: (scope: State['scope']) => void; onSeek: (seconds: number) => void
  onPlay: () => void; onPrev: () => void; onNext: () => void; onRepeat: () => void; onVolume: (volume: number) => void; onTheme: () => void
}) {
  const elapsed = playing?.elapsed ?? 0
  const shown = playing?.external ? {
    name: `${playing.name ?? 'Unknown Track'} (Spotify)`,
    art: playing.art ?? '',
    alb: playing.alb ?? '',
    durationSecs: playing.durationSecs ?? 0,
  } : track
  const duration = shown?.durationSecs ?? 0
  const volumeVisible = !connected || playing?.volumeSupported
  return <header className="transport">
    <div className="transport-controls">
      <button aria-label="Previous track" onClick={onPrev}>◀◀</button>
      <button className="play-button" aria-label={playing?.isPlaying ? 'Pause' : 'Play'} onClick={onPlay}>{playing?.isPlaying ? '❚❚' : '▶'}</button>
      <button aria-label="Next track" onClick={onNext}>▶▶</button>
      <button className={`repeat-button ${repeat !== 'off' ? 'active' : ''}`} aria-label={`Repeat: ${repeat}`} title={`Repeat: ${repeat}`} onClick={onRepeat}>⟳{repeat === 'one' && <sup>1</sup>}</button>
      {volumeVisible && <><span aria-hidden="true">🔊</span><input aria-label="Volume" type="range" min="0" max="100" value={volume} onChange={(event) => onVolume(Number(event.target.value))} /></>}
    </div>
    <div className={`lcd ${playing?.external ? 'external' : ''}`}>
      <Marquee text={shown?.name ?? 'Retune'} strong />
      <Marquee text={shown ? `${shown.art} — ${shown.alb}` : 'Not Playing'} />
      <div className="progress-row"><time>{shown ? formatTime(elapsed) : '—:—'}</time><progress
        max={duration || 1}
        value={elapsed}
        onClick={(event) => {
          if (!shown || !duration) return
          const bar = event.currentTarget.getBoundingClientRect()
          const fraction = Math.min(1, Math.max(0, (event.clientX - bar.left) / bar.width))
          onSeek(Math.round(fraction * duration))
        }}
      /><time>{shown ? `-${formatTime(Math.max(0, duration - elapsed))}` : ''}</time></div>
    </div>
    <div className="search-area">
      <div className="scope-pills" aria-label="Search scope">
        <button className={scope === 'library' ? 'active' : ''} onClick={() => onScope('library')}>Library</button>
        <button className={scope === 'spotify' ? 'active' : ''} onClick={() => onScope('spotify')}>Spotify</button>
      </div>
      <span className={`connection-dot ${connected ? 'connected' : ''}`} title={connected ? 'Spotify connected' : 'Spotify not connected'} aria-label={connected ? 'Spotify connected' : 'Spotify not connected'} />
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

function BrowserPane({ state, onActivate, onSelect }: { state: State; onActivate: (facet: keyof Selection) => void; onSelect: (facet: keyof Selection, value?: string) => void }) {
  const sourceLabels = labels[state.source].facets
  const values = [state.view?.facets.cats ?? [], state.view?.facets.arts ?? [], state.view?.facets.albs ?? []]
  const facets: (keyof Selection)[] = ['cat', 'art', 'alb']
  return <div className="browser-pane">
    {facets.map((facet, index) => <FacetColumn key={facet} facet={facet} title={sourceLabels[index]} values={values[index]} selected={state.sel[facet]} onActivate={() => onActivate(facet)} onSelect={(value) => onSelect(facet, value)} />)}
  </div>
}

function FacetColumn({ facet, title, values, selected, onActivate, onSelect }: { facet: keyof Selection; title: string; values: string[]; selected?: string; onActivate: () => void; onSelect: (value?: string) => void }) {
  return <div className="facet-column" data-facet={facet} onMouseDown={onActivate}>
    <div className="column-header">{title}</div>
    <div className="facet-list">
      <button data-row-index={0} className={!selected ? 'active' : ''} onClick={() => onSelect(undefined)}>All ({values.length} {title}s)</button>
      {values.map((value, index) => <button key={value} data-row-index={index + 1} className={selected === value ? 'active' : ''} onClick={() => onSelect(value)} title={value}>{value}</button>)}
    </div>
  </div>
}

function RatingStars({ rating, explicit = false, onRate }: { rating: number | null; explicit?: boolean; onRate: (stars: number) => void }) {
  return <span className={`rating-stars ${rating ? explicit ? 'explicit' : 'inherited' : 'empty'}`} aria-label={rating ? `${rating} out of 5 stars` : 'Unrated'}>
    {[1, 2, 3, 4, 5].map((star) => <button key={star} aria-label={`${star} stars`} onClick={(event) => { event.stopPropagation(); onRate(star) }}>{star <= (rating ?? 0) ? '★' : '☆'}</button>)}
  </span>
}

function AlbumRatingStrip({ album, rating, onRate }: { album: string; rating: number | null; onRate: (rating: number | null) => void }) {
  return <div className="album-rating-strip"><strong>{album}</strong><RatingStars rating={rating} explicit onRate={(stars) => onRate(stars === rating ? null : stars)} /></div>
}

function TrackList({ tracks, label, selectedIds, playing, columnOrder, hiddenColumns, onActivate, onSelect, onPlay, onRate, onInfo, onReorder, onHiddenColumns }: {
  tracks: Track[]; label: (typeof labels)[Source]; selectedIds: Set<number>; playing: State['playing']
  columnOrder: ColumnKey[]; hiddenColumns: ColumnKey[]; onSelect: (id: number, event: React.MouseEvent) => void; onPlay: (id: number) => void
  onRate: (id: number, stars: number) => void; onInfo: (id: number) => void; onReorder: (order: ColumnKey[]) => void
  onActivate: () => void; onHiddenColumns: (columns: ColumnKey[]) => void
}) {
  const [dragging, setDragging] = useState<ColumnKey>()
  const [menu, setMenu] = useState<{ x: number; y: number }>()
  const list = useRef<HTMLDivElement>(null)
  const headings: Record<ColumnKey, string> = {
    track: '#',
    name: label.item[0].toUpperCase() + label.item.slice(1),
    time: 'Time',
    artist: label.facets[1],
    album: label.facets[2],
    genre: label.facets[0],
    rating: 'Rating',
  }
  const widths: Record<ColumnKey, string> = { track: '34px', name: 'minmax(160px, 1.6fr)', time: '52px', artist: '1.1fr', album: '1.1fr', genre: '.9fr', rating: '84px' }
  const visibleColumns = columnOrder.filter((column) => !hiddenColumns.includes(column))
  const columns = `22px ${visibleColumns.map((column) => widths[column]).join(' ')}`
  useEffect(() => {
    if (!menu) return
    const close = (event: PointerEvent) => {
      if (!(event.target as HTMLElement).closest('.column-menu')) setMenu(undefined)
    }
    const escape = (event: KeyboardEvent) => { if (event.key === 'Escape') setMenu(undefined) }
    document.addEventListener('pointerdown', close)
    document.addEventListener('keydown', escape)
    return () => {
      document.removeEventListener('pointerdown', close)
      document.removeEventListener('keydown', escape)
    }
  }, [menu])
  const drop = (target: ColumnKey) => {
    if (!dragging || dragging === target) return
    const next = columnOrder.filter((column) => column !== dragging)
    next.splice(next.indexOf(target), 0, dragging)
    onReorder(next)
  }
  const cell = (track: Track, column: ColumnKey) => {
    if (column === 'track') return <span key={column}>{track.trackNo ?? ''}</span>
    if (column === 'name') return <span key={column} className="track-name" title={track.name}>{track.name}{selectedIds.has(track.id) && <button className="info-button" aria-label={`Get info for ${track.name}`} onClick={(event) => { event.stopPropagation(); onInfo(track.id) }}>ⓘ</button>}</span>
    if (column === 'time') return <span key={column}>{formatTime(track.durationSecs)}</span>
    if (column === 'artist') return <span key={column} title={track.art}>{track.art}</span>
    if (column === 'album') return <span key={column} title={track.alb}>{track.alb}</span>
    if (column === 'genre') return <span key={column} title={track.cat}>{track.overridden ? '● ' : ''}{track.cat}</span>
    return <RatingStars key={column} rating={track.rating?.stars ?? null} explicit={track.rating?.explicit} onRate={(stars) => onRate(track.id, stars)} />
  }
  return <div className="track-list" ref={list} onMouseDown={onActivate}>
    <div className="track-row track-header" style={{ gridTemplateColumns: columns }} onContextMenu={(event) => {
      event.preventDefault()
      const bounds = list.current?.getBoundingClientRect()
      setMenu({ x: event.clientX - (bounds?.left ?? 0), y: event.clientY - (bounds?.top ?? 0) })
    }}><span />{visibleColumns.map((column) => <span key={column} draggable className={dragging === column ? 'dragging' : ''} onDragStart={(event) => {
      setDragging(column)
      event.dataTransfer.effectAllowed = 'move'
    }} onDragEnd={() => setDragging(undefined)} onDragOver={(event) => event.preventDefault()} onDrop={() => drop(column)}>{headings[column]}</span>)}</div>
    <div className="track-scroll">
      {tracks.map((track) => {
        const isPlaying = playing?.trackId === track.id
        return <div key={track.id} data-track-id={track.id} className={`track-row ${selectedIds.has(track.id) ? 'selected' : ''} ${isPlaying ? 'playing' : ''}`} style={{ gridTemplateColumns: columns }} onClick={(event) => onSelect(track.id, event)} onDoubleClick={() => onPlay(track.id)}>
          <span className="playing-marker">{isPlaying ? playing.isPlaying ? '▶' : '❚❚' : ''}</span>
          {visibleColumns.map((column) => cell(track, column))}
        </div>
      })}
    </div>
    {menu && <div className="column-menu" style={{ left: menu.x, top: menu.y }}>
      {columnOrder.map((column) => <label key={column}><input type="checkbox" checked={!hiddenColumns.includes(column)} disabled={column === 'name'} onChange={(event) => onHiddenColumns(event.target.checked
        ? hiddenColumns.filter((hidden) => hidden !== column)
        : [...hiddenColumns, column])} />{headings[column]}</label>)}
    </div>}
  </div>
}

function SpotifySearch({ searching, results, onArtist, onAdd }: {
  searching: boolean
  results: SpotifyResults | null
  onArtist: (artist: SpotifyResults['artists'][number]) => void
  onAdd: (album: SpotifyResults['albums'][number]) => Promise<unknown>
}) {
  const [adding, setAdding] = useState<string>()
  const add = (album: SpotifyResults['albums'][number]) => {
    setAdding(album.uri)
    void onAdd(album).finally(() => setAdding(undefined))
  }
  if (searching) return <div className="spotify-stub">Searching Spotify…</div>
  return <div className="spotify-results">
    <section><h2>Artists</h2>{results?.artists.map((artist) => <button className="spotify-row" key={artist.uri} onClick={() => onArtist(artist)}><span>{artist.name}</span><span>View albums ›</span></button>)}{!results?.artists.length && <p>No artists found.</p>}</section>
    <section><h2>Albums</h2>{results?.albums.map((album) => <div className="spotify-row" key={album.uri}><span><strong>{album.name}</strong><small>{album.artist}{album.trackCount ? ` · ${album.trackCount} tracks` : ''}</small></span><button disabled={adding === album.uri} onClick={() => add(album)}>{adding === album.uri ? 'Adding…' : '+ Add'}</button></div>)}{!results?.albums.length && <p>No albums found.</p>}</section>
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
      const ratingChange = { stars: rating?.explicit ? rating.stars : null }
      await invoke('edit_track', { id: track.id, edit: { ...draft, ratingChange } })
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

function MultipleItemInformation({ tracks, onCancel, onSaved, onError }: { tracks: Track[]; onCancel: () => void; onSaved: () => void; onError: (error: string) => void }) {
  type Field = 'art' | 'alb' | 'cat'
  const [draft, setDraft] = useState<Partial<Record<Field, string>>>({})
  const [rating, setRating] = useState<number | null | undefined>(undefined)
  const dialog = useRef<HTMLDivElement>(null)
  useEffect(() => { dialog.current?.focus() }, [])
  const placeholder = (key: Field) => tracks.every((track) => track[key] === tracks[0][key]) ? tracks[0][key] : 'Mixed'
  const save = async () => {
    try {
      await invoke('set_track_infos', {
        ids: tracks.map((track) => track.id),
        edit: { ...draft, ...(rating === undefined ? {} : { ratingChange: { stars: rating } }) },
      })
      onSaved()
    } catch (error) {
      onError(String(error))
    }
  }
  const field = (key: Field) => ({
    value: draft[key] ?? '',
    placeholder: placeholder(key),
    onChange: (event: React.ChangeEvent<HTMLInputElement>) => setDraft({ ...draft, [key]: event.target.value }),
  })
  return <div className="modal-backdrop" role="presentation">
    <div className="get-info" role="dialog" aria-modal="true" aria-labelledby="multiple-item-information-title" tabIndex={-1} ref={dialog}>
      <h2 id="multiple-item-information-title">Editing {tracks.length} items</h2>
      <label>Artist<input {...field('art')} /></label>
      <label>Album<input {...field('alb')} /></label>
      <label>Genre<input {...field('cat')} /></label>
      <div className="info-rating bulk-rating"><span>Rating</span><RatingStars rating={rating ?? null} explicit={rating !== undefined && rating !== null} onRate={setRating} /><button className={rating === undefined ? 'active' : ''} onClick={() => setRating(undefined)}>No Change</button><button className={rating === null ? 'active' : ''} onClick={() => setRating(null)}>Clear</button></div>
      <div className="modal-actions"><button onClick={onCancel}>Cancel</button><button className="primary" onClick={() => void save()}>Save Overlay</button></div>
    </div>
  </div>
}

function Preferences({ settings, onCancel, onSave }: { settings: Settings; onCancel: () => void; onSave: (theme: Theme, autoAdd: boolean, autoConnect: boolean, clientId: string, playbackBackend: PlaybackBackend) => void }) {
  const [theme, setTheme] = useState(settings.theme)
  const [autoAdd, setAutoAdd] = useState(settings.autoAddSpotifyLibrary)
  const [autoConnect, setAutoConnect] = useState(settings.autoConnect)
  const [clientId, setClientId] = useState(settings.spotifyClientId)
  const [playbackBackend, setPlaybackBackend] = useState(settings.playbackBackend)
  const dialog = useRef<HTMLDivElement>(null)
  useEffect(() => { dialog.current?.focus() }, [])
  return <div className="modal-backdrop" role="presentation">
    <div className="get-info preferences" role="dialog" aria-modal="true" aria-labelledby="preferences-title" tabIndex={-1} ref={dialog}>
      <h2 id="preferences-title">Preferences</h2>
      <fieldset>
        <legend>Appearance</legend>
        {(['system', 'light', 'dark'] as Theme[]).map((value) => <label className="radio" key={value}><input type="radio" name="theme" value={value} checked={theme === value} onChange={() => setTheme(value)} />{value[0].toUpperCase() + value.slice(1)}</label>)}
      </fieldset>
      <fieldset>
        <legend>Library</legend>
        <label>Spotify Client ID<input value={clientId} onChange={(event) => setClientId(event.target.value)} placeholder="From developer.spotify.com" /></label>
        <label className="checkbox"><input type="checkbox" checked={autoAdd} onChange={(event) => setAutoAdd(event.target.checked)} />Automatically add my entire Spotify library</label>
        <label className="checkbox"><input type="checkbox" checked={autoConnect} onChange={(event) => setAutoConnect(event.target.checked)} />Connect to Spotify automatically at launch</label>
        <p>Keep pulling in music you add on Spotify each time Retune starts.</p>
      </fieldset>
      <fieldset>
        <legend>Playback</legend>
        <label className="radio"><input type="radio" name="playback-backend" checked={playbackBackend === 'connect'} onChange={() => setPlaybackBackend('connect')} />Spotify app (Connect)</label>
        <label className="radio"><input type="radio" name="playback-backend" checked={playbackBackend === 'local'} onChange={() => setPlaybackBackend('local')} />Built-in (librespot)</label>
      </fieldset>
      <div className="modal-actions"><button onClick={onCancel}>Cancel</button><button className="primary" onClick={() => onSave(theme, autoAdd, autoConnect, clientId.trim(), playbackBackend)}>Save</button></div>
    </div>
  </div>
}

function StatusBar({ view, unit, syncPhase }: { view: BrowseView | null; unit: string; syncPhase?: string }) {
  const total = view?.counts.totalSecs ?? 0
  const hours = Math.floor(total / 3600)
  const minutes = Math.floor((total % 3600) / 60)
  const count = view?.counts.tracks ?? 0
  return <footer className="status-bar"><button aria-label="Add">+</button><span>{syncPhase ?? `${count} ${count === 1 ? unit : `${unit}s`}, ${hours}:${String(minutes).padStart(2, '0')} hours`}</span><span>{view?.counts.overlayEdits ?? 0} overlay edits</span></footer>
}

export default App
