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
type Selection = { cat?: string[]; art?: string[]; alb?: string[] }
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
  streamingBitrate: number
  normalizeVolume: boolean
  gapless: boolean
}

type ConnectionState = { connected: boolean }
export type PlaylistListView = {
  id: string
  name: string
  owned: boolean
  owner: string | null
  contains: boolean
  trackCount: number
}
type RatingView = { stars: number; explicit: boolean }
type SearchAlbum = {
  uri: string
  name: string
  artist: string
  year: string | null
  imageUrl: string | null
  albumType: string | null
}
type SpotifyResults = {
  artists: { id: string; name: string; descriptor: string; imageUrl: string | null }[]
  albums: SearchAlbum[]
  tracks: { uri: string; name: string; artist: string; alb: string; durationSecs: number; imageUrl: string | null; albumUri: string | null }[]
}

export type AlbumPageView = {
  uri: string
  name: string
  artist: string
  artistId: string
  albumType: string
  year: string | null
  imageUrl: string | null
  totalDurationSecs: number
  inLibrary: boolean
  albumRating: number | null
  tracks: {
    uri: string
    name: string
    trackNo: number | null
    durationSecs: number
    trackId: number | null
    rating: RatingView | null
  }[]
}

export type ArtistPageView = {
  id: string
  name: string
  descriptor: string
  imageUrl: string | null
  following: boolean
  albums: SearchAlbum[]
  topTracks: {
    uri: string
    name: string
    alb: string
    durationSecs: number
    imageUrl: string | null
    albumUri: string
  }[]
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
  rating: RatingView | null
}

type PlaybackTrack = Pick<Track, 'id' | 'uri' | 'name' | 'art' | 'alb' | 'durationSecs'>
type PlaylistTrack = Omit<PlaybackTrack, 'id'> & { id: number | null; rating: RatingView | null }
type PlaylistSubject =
  | { kind: 'tracks'; label: string; uris: string[] }
  | { kind: 'album'; label: string; albumUri: string }

const DRAG_TYPE = 'application/x-retune'

type PlayerState = {
  trackId: number | null
  uri: string | null
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
type Playing = PlayerState & { queue: readonly PlaybackTrack[]; simulated?: boolean }

type BrowseView = {
  facets: { cats: string[]; arts: string[]; albs: string[] }
  tracks: Track[]
  albumRating: number | null
  albumRatingArtist: string | null
  albumRatingAmbiguous: boolean
  counts: {
    tracks: number
    totalSecs: number
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
  rating: RatingView | null
  inheritedRating: number | null
  genres: string[]
}

type MetadataValues = { arts: string[]; albs: string[]; cats: string[] }

type InfoDialog =
  | { kind: 'single'; track: TrackInfo }
  | { kind: 'multiple'; tracks: Track[] }

const emptyTracks: Track[] = []
const ZOOM_MIN = 0.7
const ZOOM_MAX = 1.8
const streamingQualities = [
  ['Normal', 96],
  ['High', 160],
  ['Very High', 320],
] as const

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
  setup: boolean
  connected: boolean
  spotifyResults: SpotifyResults | null
  spotifySearching: boolean
  selectedPlaylist?: string
  playlistRevision: number
  syncPhase?: string
  syncProgress?: { tracks: number; fraction: number }
}

type Action =
  | { type: 'view'; view: BrowseView }
  | { type: 'error'; error: string }
  | { type: 'clear-error' }
  | { type: 'source'; source: Source }
  | { type: 'playlist'; id?: string }
  | { type: 'select'; facet: keyof Selection; values: string[] }
  | { type: 'query'; query: string }
  | { type: 'scope'; scope: State['scope'] }
  | { type: 'selectTrack'; id: number }
  | { type: 'selection'; ids: Set<number>; anchor?: number }
  | { type: 'play'; id: number; queue: readonly PlaybackTrack[] }
  | { type: 'togglePlay' }
  | { type: 'step'; id: number }
  | { type: 'tick'; duration: number; nextId: number }
  | { type: 'seek'; elapsed: number }
  | { type: 'playerState'; player: PlayerState; queue: readonly PlaybackTrack[] }
  | { type: 'hydrateSettings'; settings: Settings }
  | { type: 'settings'; settings: Partial<Settings> }
  | { type: 'systemTheme'; dark: boolean }
  | { type: 'refresh' }
  | { type: 'notice'; notice?: string }
  | { type: 'info'; info?: InfoDialog }
  | { type: 'preferences'; open: boolean }
  | { type: 'setup'; open: boolean }
  | { type: 'connection'; connected: boolean }
  | { type: 'spotifyResults'; results: SpotifyResults | null }
  | { type: 'spotifySearching'; searching: boolean }
  | { type: 'syncPhase'; phase?: string }
  | { type: 'syncProgress'; progress: { tracks: number; fraction: number } }
  | { type: 'playlistsRefresh' }

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
  streamingBitrate: 320,
  normalizeVolume: false,
  gapless: true,
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
  setup: false,
  connected: false,
  spotifyResults: null,
  spotifySearching: false,
  playlistRevision: 0,
}

function reducer(state: State, action: Action): State {
  switch (action.type) {
    case 'view':
      return { ...state, view: action.view, error: undefined }
    case 'error':
      return { ...state, error: action.error, syncProgress: undefined }
    case 'clear-error':
      return { ...state, error: undefined }
    case 'source':
      return { ...state, source: action.source, sel: {}, query: '', selectedPlaylist: undefined, selectedTrackIds: new Set(), selectionAnchor: undefined }
    case 'playlist':
      return { ...state, selectedPlaylist: action.id, sel: {}, selectedTrackIds: new Set(), selectionAnchor: undefined }
    case 'select': {
      const sel = action.facet === 'cat'
        ? { cat: action.values }
        : action.facet === 'art'
          ? { cat: state.sel.cat, art: action.values }
          : { ...state.sel, alb: action.values }
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
          uri: action.queue.find((track) => track.id === action.id)?.uri ?? null,
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
      return { ...state, info: action.info, preferences: false, setup: false }
    case 'preferences':
      return { ...state, preferences: action.open, setup: false, info: undefined }
    case 'setup':
      return { ...state, setup: action.open, preferences: false, info: undefined }
    case 'connection':
      return { ...state, connected: action.connected }
    case 'spotifyResults':
      return { ...state, spotifyResults: action.results, spotifySearching: false }
    case 'spotifySearching':
      return { ...state, spotifySearching: action.searching }
    case 'syncPhase':
      return { ...state, syncPhase: action.phase, syncProgress: action.phase ? state.syncProgress : undefined }
    case 'syncProgress':
      return { ...state, syncProgress: action.progress.fraction < 1 ? action.progress : undefined }
    case 'playlistsRefresh':
      return { ...state, playlistRevision: state.playlistRevision + 1 }
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
  const queue = useRef<readonly PlaybackTrack[]>(emptyTracks)
  const pendingPlay = useRef<{ id: number; tracks: readonly PlaybackTrack[] } | null>(null)
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

  const start = useCallback((id: number, tracks: readonly PlaybackTrack[]) => {
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
  const [playlists, setPlaylists] = useState<PlaylistListView[]>()
  const [playlistSubject, setPlaylistSubject] = useState<PlaylistSubject>()
  const search = useRef<HTMLInputElement>(null)
  const preferenceZoom = useRef(defaultSettings.zoom)
  const skipSettingsSave = useRef(false)
  const facetAnchors = useRef<Partial<Record<keyof Selection, string>>>({})
  const typeahead = useRef({ buffer: '', timer: 0 })
  const typeaheadExpires = useRef(0)
  const view = state.view
  const tracks = view?.tracks ?? emptyTracks
  const selectedTracks = tracks.filter((track) => state.selectedTrackIds.has(track.id))
  const spotifySearchActive = state.scope === 'spotify' && Boolean(state.query.trim())
  const tracklistVisible = !spotifySearchActive && !state.selectedPlaylist
  const libraryEmpty = view?.counts.perSource[state.source] === 0 && !state.syncPhase && !state.syncProgress
  const playbackTracks = state.playing?.queue ?? emptyTracks
  const player = usePlayer(state.connected, state.playing, dispatch)
  const addToPlaylist = useCallback((id: string, subject: PlaylistSubject) => subject.kind === 'album'
    ? invoke('playlist_add_album', { id, albumUri: subject.albumUri })
    : invoke('playlist_add', { id, uris: subject.uris }), [])
  const selectFacet = useCallback((facet: keyof Selection, values: string[], anchor?: string) => {
    facetAnchors.current[facet] = anchor
    if (facet === 'cat') {
      delete facetAnchors.current.art
      delete facetAnchors.current.alb
    } else if (facet === 'art') {
      delete facetAnchors.current.alb
    }
    dispatch({ type: 'select', facet, values })
  }, [])
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
      sel: { cat: state.sel.cat ?? [], art: state.sel.art ?? [], alb: state.sel.alb ?? [] },
      query: state.scope === 'library' && state.query.trim() ? state.query : undefined,
    }).then((next) => active && dispatch({ type: 'view', view: next }))
      .catch((error) => active && dispatch({ type: 'error', error: String(error) }))
    return () => { active = false }
  }, [state.source, state.sel, state.query, state.scope, state.revision])

  useEffect(() => {
    let active = true
    invoke<PlaylistListView[]>('playlists_list')
      .then((rows) => active && setPlaylists(rows))
      .catch((error) => active && dispatch({ type: 'error', error: String(error) }))
    return () => { active = false }
  }, [state.playlistRevision])

  useEffect(() => {
    if (playlists && state.selectedPlaylist && !playlists.some((playlist) => playlist.id === state.selectedPlaylist)) {
      dispatch({ type: 'playlist' })
    }
  }, [playlists, state.selectedPlaylist])

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
    if (!state.settingsHydrated || state.preferences) return
    if (skipSettingsSave.current) {
      skipSettingsSave.current = false
      return
    }
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
    state.preferences,
  ])

  useEffect(() => {
    const unlisten = listen('get-info', () => openInfo())
    return () => { void unlisten.then((stop) => stop()) }
  }, [state.selectedTrackIds, tracks])

  useEffect(() => {
    const changed = listen('library-changed', () => dispatch({ type: 'refresh' }))
    const failed = listen<string>('operation-error', ({ payload }) => dispatch({ type: 'error', error: payload }))
    const recovered = listen('operation-recovered', () => dispatch({ type: 'clear-error' }))
    const connection = listen<ConnectionState>('connection-changed', ({ payload }) => dispatch({ type: 'connection', connected: payload.connected }))
    const settings = listen<Settings>('settings-changed', ({ payload }) => dispatch({ type: 'hydrateSettings', settings: payload }))
    const progress = listen<string>('sync-progress', ({ payload }) => dispatch({ type: 'syncPhase', phase: payload || undefined }))
    const progressCount = listen<{ tracks: number; fraction: number }>('sync-progress-count', ({ payload }) => dispatch({ type: 'syncProgress', progress: payload }))
    const playlistsChanged = listen('playlists-changed', () => dispatch({ type: 'playlistsRefresh' }))
    return () => {
      void changed.then((stop) => stop())
      void failed.then((stop) => stop())
      void recovered.then((stop) => stop())
      void connection.then((stop) => stop())
      void settings.then((stop) => stop())
      void progress.then((stop) => stop())
      void progressCount.then((stop) => stop())
      void playlistsChanged.then((stop) => stop())
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
    settings: { zoom: Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, Math.round(zoom * 10) / 10)) },
  })
  const openPreferences = () => {
    preferenceZoom.current = state.settings.zoom
    dispatch({ type: 'preferences', open: true })
  }
  const cancelPreferences = () => {
    skipSettingsSave.current = true
    setZoom(preferenceZoom.current)
    dispatch({ type: 'preferences', open: false })
  }
  const saveSetupClientId = async (spotifyClientId: string) => {
    if (spotifyClientId === state.settings.spotifyClientId) return
    const settings = { ...state.settings, spotifyClientId }
    dispatch({ type: 'settings', settings: { spotifyClientId } })
    await invoke('set_settings', { settings })
  }
  const cycleTheme = () => dispatch({
    type: 'settings',
    settings: { theme: state.settings.theme === 'system' ? 'light' : state.settings.theme === 'light' ? 'dark' : 'system' },
  })
  const playingTrack = playbackTracks.find((track) => track.id === state.playing?.trackId)
  const selectedAlbum = state.sel.alb?.length === 1 ? state.sel.alb[0] : undefined

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
    const preferences = listen('open-preferences', openPreferences)
    const setup = listen('open-setup', () => dispatch({ type: 'setup', open: true }))
    return () => {
      void viewActions.then((stop) => stop())
      void playerActions.then((stop) => stop())
      void preferences.then((stop) => stop())
      void setup.then((stop) => stop())
    }
  }, [state.settings, player])

  useEffect(() => {
    if (typeahead.current.buffer) {
      const remaining = typeaheadExpires.current - Date.now()
      if (remaining > 0) typeahead.current.timer = window.setTimeout(() => { typeahead.current.buffer = '' }, remaining)
      else typeahead.current.buffer = ''
    }
    const onKeyDown = (event: KeyboardEvent) => {
      const modalOpen = Boolean(state.info || state.preferences || state.setup || playlistSubject)
      if (event.key === 'Escape' && modalOpen) {
        event.preventDefault()
        if (state.info) dispatch({ type: 'info' })
        else if (state.preferences) cancelPreferences()
        else if (state.setup) dispatch({ type: 'setup', open: false })
        else setPlaylistSubject(undefined)
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
        openPreferences()
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
          selectFacet(activePane, [facetValues[index]], facetValues[index])
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
          const current = values.indexOf(facetAnchors.current[activePane] ?? state.sel[activePane]?.[0])
          const index = Math.max(0, Math.min(values.length - 1, current + direction))
          selectFacet(activePane, values[index] === undefined ? [] : [values[index]], values[index])
          window.requestAnimationFrame(() => document.querySelector<HTMLElement>(`[data-facet="${activePane}"] [data-row-index="${index}"]`)?.scrollIntoView({ block: 'nearest' }))
        }
      }
    }
    window.addEventListener('keydown', onKeyDown)
    return () => {
      window.clearTimeout(typeahead.current.timer)
      window.removeEventListener('keydown', onKeyDown)
    }
  }, [activePane, playlistSubject, state.info, state.preferences, state.setup, state.sel, state.selectedTrackIds, state.selectionAnchor, state.settings.zoom, state.view, tracks, tracklistVisible, player, selectFacet])

  useEffect(() => {
    const onWheel = (event: WheelEvent) => {
      if (!(event.metaKey || event.ctrlKey) || state.info || state.preferences || state.setup || playlistSubject) return
      event.preventDefault()
      setZoom(state.settings.zoom + (event.deltaY < 0 ? 0.1 : -0.1))
    }
    window.addEventListener('wheel', onWheel, { passive: false })
    return () => window.removeEventListener('wheel', onWheel)
  }, [playlistSubject, state.info, state.preferences, state.setup, state.settings.zoom])

  const selectedPlaylist = playlists?.find((playlist) => playlist.id === state.selectedPlaylist)

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
        <Sidebar
          state={state}
          playlists={playlists}
          onSource={(source) => { facetAnchors.current = {}; dispatch({ type: 'source', source }) }}
          onPlaylist={(id) => dispatch({ type: 'playlist', id })}
          onDrop={(id, subject) => addToPlaylist(id, subject).catch((error) => dispatch({ type: 'error', error: String(error) }))}
          onError={(error) => dispatch({ type: 'error', error })}
        />
        <section className="content">
          {spotifySearchActive ? (
            state.connected ? <SpotifySearch
              query={state.query.trim()}
              searching={state.spotifySearching}
              results={state.spotifyResults}
              onAdd={(album) => invoke('add_spotify_album', album)
                .catch((error) => {
                  dispatch({ type: 'error', error: String(error) })
                  throw error
                })}
              onPlay={player.start}
              onPlaylist={setPlaylistSubject}
              onError={(error) => dispatch({ type: 'error', error })}
            /> : <div className="spotify-stub"><span>Connect to Spotify to search artists and albums.</span><button onClick={() => invoke('connect_spotify').catch((error) => dispatch({ type: 'error', error: String(error) }))}>Connect to Spotify</button></div>
          ) : selectedPlaylist ? <PlaylistView playlist={selectedPlaylist} revision={state.playlistRevision} playing={state.playing} onPlay={player.start} onError={(error) => dispatch({ type: 'error', error })} />
          : (
            <>
              <BrowserPane state={state} anchors={facetAnchors} onActivate={setActivePane} onSelect={selectFacet} />
              {selectedAlbum !== undefined && view && !view.albumRatingAmbiguous && view.albumRatingArtist !== null && (
                <AlbumRatingStrip
                  album={selectedAlbum}
                  rating={view?.albumRating ?? null}
                  onRate={(stars) => mutate('set_album_rating', {
                    source: state.source,
                    art: view.albumRatingArtist,
                    alb: selectedAlbum,
                    stars,
                  })}
                />
              )}
              {state.notice && <div className="startup-notice"><span>{state.notice}</span><button aria-label="Dismiss notice" onClick={() => dispatch({ type: 'notice' })}>×</button></div>}
              <TrackList
                tracks={tracks}
                label={labels[state.source]}
                selectedIds={state.selectedTrackIds}
                playing={state.playing}
                columnOrder={state.settings.columnOrder}
                hiddenColumns={state.settings.hiddenColumns}
                empty={libraryEmpty}
                onActivate={() => setActivePane('track')}
                onSetup={() => dispatch({ type: 'setup', open: true })}
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
                onPlaylist={setPlaylistSubject}
                onReorder={(columnOrder) => dispatch({ type: 'settings', settings: { columnOrder } })}
                onHiddenColumns={(hiddenColumns) => dispatch({ type: 'settings', settings: { hiddenColumns } })}
              />
            </>
          )}
          {state.error && <div className="error-banner">{state.error}</div>}
          <StatusBar view={view} unit={labels[state.source].item} syncPhase={state.syncPhase} syncProgress={state.syncProgress} empty={libraryEmpty} />
        </section>
      </div>
      {state.info?.kind === 'single' && <GetInfo key={state.info.track.id} track={state.info.track} onCancel={() => dispatch({ type: 'info' })} onSaved={() => {
        dispatch({ type: 'info' })
        dispatch({ type: 'refresh' })
      }} onError={(error) => dispatch({ type: 'error', error })} />}
      {state.info?.kind === 'multiple' && <MultipleItemInformation tracks={state.info.tracks} onCancel={() => dispatch({ type: 'info' })} onSaved={() => dispatch({ type: 'info' })} onError={(error) => dispatch({ type: 'error', error })} />}
      {state.setup && <SetupLibrary settings={state.settings} connected={state.connected} onCancel={() => dispatch({ type: 'setup', open: false })} onConnect={(clientId) => saveSetupClientId(clientId)
        .then(() => invoke('connect_spotify'))
        .catch((error) => dispatch({ type: 'error', error: String(error) }))} onSync={(clientId) => saveSetupClientId(clientId)
        .then(() => {
          dispatch({ type: 'setup', open: false })
          return invoke('sync_from_spotify')
        })
        .catch((error) => dispatch({ type: 'error', error: String(error) }))} />}
      {state.preferences && <Preferences settings={state.settings} onZoom={setZoom} onCancel={cancelPreferences} onSave={(theme, autoAddSpotifyLibrary, autoConnect, spotifyClientId, playbackBackend, streamingBitrate, normalizeVolume, gapless) => {
        const audioChanged = streamingBitrate !== state.settings.streamingBitrate
          || normalizeVolume !== state.settings.normalizeVolume
          || gapless !== state.settings.gapless
        dispatch({
          type: 'settings',
          settings: { theme, autoAddSpotifyLibrary, autoConnect, spotifyClientId, playbackBackend, streamingBitrate, normalizeVolume, gapless },
        })
        if (audioChanged) invoke('set_audio_settings', { streamingBitrate, normalizeVolume, gapless })
          .catch((error) => dispatch({ type: 'error', error: String(error) }))
        dispatch({ type: 'preferences', open: false })
      }} />}
      {playlistSubject && <AddToPlaylist subject={playlistSubject} revision={state.playlistRevision} onAdd={addToPlaylist} onClose={() => setPlaylistSubject(undefined)} onError={(error) => dispatch({ type: 'error', error })} />}
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

const artworkCache = new Map<string, string | null>()

function TransportBar({ playing, track, query, scope, theme, connected, volume, repeat, searchRef, onQuery, onScope, onPlay, onPrev, onNext, onRepeat, onVolume, onSeek, onTheme }: {
  playing: State['playing']; track?: PlaybackTrack; query: string; scope: State['scope']; theme: Theme
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
  const uri = playing?.external ? playing.uri : track?.uri
  const [artwork, setArtwork] = useState<string | null>(null)
  useEffect(() => {
    let current = true
    if (!uri) {
      setArtwork(null)
      return () => { current = false }
    }
    if (artworkCache.has(uri)) {
      setArtwork(artworkCache.get(uri) ?? null)
      return () => { current = false }
    }
    setArtwork(null)
    invoke<string | null>('track_artwork', { uri })
      .then((url) => {
        artworkCache.set(uri, url)
        if (current) setArtwork(url)
      })
      .catch(() => {
        artworkCache.set(uri, null)
        if (current) setArtwork(null)
      })
    return () => { current = false }
  }, [uri])
  const volumeVisible = !connected || playing?.volumeSupported
  return <header className="transport">
    <div className="transport-controls">
      <button aria-label="Previous track" onClick={onPrev}>◀◀</button>
      <button className="play-button" aria-label={playing?.isPlaying ? 'Pause' : 'Play'} onClick={onPlay}>{playing?.isPlaying ? '❚❚' : '▶'}</button>
      <button aria-label="Next track" onClick={onNext}>▶▶</button>
      <button className={`repeat-button ${repeat !== 'off' ? 'active' : ''}`} aria-label={`Repeat: ${repeat}`} title={`Repeat: ${repeat}`} onClick={onRepeat}>⟳{repeat === 'one' && <sup>1</sup>}</button>
      {volumeVisible && <><span aria-hidden="true">🔊</span><input aria-label="Volume" type="range" min="0" max="100" value={volume} onChange={(event) => onVolume(Number(event.target.value))} /></>}
    </div>
    <div className={`lcd ${playing?.external ? 'external' : ''} ${shown ? '' : 'idle'}`}>
      <div className="lcd-artwork">{artwork ? <img src={artwork} alt="" /> : <span aria-hidden="true">♪</span>}</div>
      <div className="lcd-copy">
        <Marquee text={shown?.name ?? 'Retune'} strong />
        <div className="lcd-meta">{shown ? <><span className="lcd-artist">{shown.art}</span><span className="lcd-album"> · {shown.alb}</span></> : 'Not Playing'}</div>
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

function Sidebar({ state, playlists, onSource, onPlaylist, onDrop, onError }: {
  state: State
  playlists?: PlaylistListView[]
  onSource: (source: Source) => void
  onPlaylist: (id: string) => void
  onDrop: (id: string, subject: PlaylistSubject) => void
  onError: (error: string) => void
}) {
  const [creating, setCreating] = useState(false)
  const [name, setName] = useState('')
  const [dropTarget, setDropTarget] = useState<string>()
  const create = async () => {
    if (!name.trim()) return
    try {
      await invoke('playlist_create', { name })
      setName('')
      setCreating(false)
    } catch (error) {
      onError(String(error))
    }
  }
  return <aside className="sidebar">
    <div className="section-label">Library</div>
    {(Object.keys(labels) as Source[]).map((source) => <button key={source} className={`source-row ${state.source === source && !state.selectedPlaylist ? 'active' : ''}`} onClick={() => onSource(source)}>
      <span>{labels[source].icons}</span><span>{labels[source].name}</span><span className="source-count">{state.view?.counts.perSource[source] ?? '—'}</span>
    </button>)}
    <div className="section-label playlists-label"><span>Playlists</span><button aria-label="New playlist" onClick={() => setCreating(true)}>+</button></div>
    {creating && <div className="playlist-new-row"><input autoFocus aria-label="Playlist name" value={name} onChange={(event) => setName(event.target.value)} onKeyDown={(event) => {
      if (event.key === 'Enter') void create()
      else if (event.key === 'Escape') { setCreating(false); setName('') }
    }} /></div>}
    {playlists?.map((playlist) => <button
      key={playlist.id}
      className={`playlist-row ${state.selectedPlaylist === playlist.id || dropTarget === playlist.id ? 'active' : ''}`}
      onClick={() => onPlaylist(playlist.id)}
      onDragOver={playlist.owned ? (event) => {
        if (!event.dataTransfer.types.includes(DRAG_TYPE)) return
        event.preventDefault()
        event.dataTransfer.dropEffect = 'copy'
        setDropTarget(playlist.id)
      } : undefined}
      onDragLeave={(event) => { if (!event.currentTarget.contains(event.relatedTarget as Node | null)) setDropTarget(undefined) }}
      onDrop={playlist.owned ? (event) => {
        event.preventDefault()
        setDropTarget(undefined)
        try { onDrop(playlist.id, JSON.parse(event.dataTransfer.getData(DRAG_TYPE)) as PlaylistSubject) }
        catch { onError('Could not read the dragged playlist item.') }
      } : undefined}
    >
      <span>{playlist.owned ? '' : '🌐'}</span><span title={playlist.name}>{playlist.name}</span><span className="source-count">{playlist.trackCount}</span>
    </button>)}
  </aside>
}

function PlaylistView({ playlist, revision, playing, onPlay, onError }: {
  playlist: PlaylistListView
  revision: number
  playing: State['playing']
  onPlay: (id: number, tracks: readonly PlaybackTrack[]) => void
  onError: (error: string) => void
}) {
  const [tracks, setTracks] = useState<PlaylistTrack[]>([])
  const [selected, setSelected] = useState<Set<number>>(new Set())
  const [selectionAnchor, setSelectionAnchor] = useState<number>()
  const [dragging, setDragging] = useState<{ start: number; length: number }>()
  const [insertBefore, setInsertBefore] = useState<number>()
  const [mutating, setMutating] = useState(false)
  const canReorder = playlist.owned && tracks.length === playlist.trackCount
  useEffect(() => {
    let active = true
    setSelected(new Set())
    setSelectionAnchor(undefined)
    invoke<PlaylistTrack[]>('playlist_tracks', { id: playlist.id })
      .then((rows) => active && setTracks(rows))
      .catch((error) => active && onError(String(error)))
    return () => { active = false }
  }, [playlist.id, revision])
  const queue: PlaybackTrack[] = tracks.map((track, index) => ({ ...track, id: track.id ?? SYNTHETIC_BASE + index }))
  const drop = async (index: number) => {
    if (dragging === undefined) return
    if (index >= dragging.start && index <= dragging.start + dragging.length) {
      setDragging(undefined)
      setInsertBefore(undefined)
      return
    }
    setMutating(true)
    setDragging(undefined)
    setInsertBefore(undefined)
    try {
      await invoke('playlist_reorder', { id: playlist.id, rangeStart: dragging.start, insertBefore: index, rangeLength: dragging.length })
    } catch (error) {
      onError(String(error))
    } finally {
      setSelected(new Set())
      setSelectionAnchor(undefined)
      setMutating(false)
    }
  }
  const remove = async () => {
    const indices = [...selected].sort((left, right) => left - right)
    if (!indices.length || !window.confirm(`Remove ${indices.length} selected ${indices.length === 1 ? 'track' : 'tracks'} from “${playlist.name}”?`)) return
    setMutating(true)
    try {
      await invoke('playlist_remove', { id: playlist.id, indices })
    } catch (error) {
      onError(String(error))
    } finally {
      setSelected(new Set())
      setSelectionAnchor(undefined)
      setMutating(false)
    }
  }
  return <div className="playlist-view">
    <header className="playlist-header"><strong>{playlist.name}</strong><span>{playlist.trackCount} {playlist.trackCount === 1 ? 'track' : 'tracks'}{playlist.owner ? ` · by ${playlist.owner}` : ''}</span><button disabled={!canReorder || !selected.size || mutating} onClick={() => void remove()}>Remove</button></header>
    <div className="playlist-track-header"><span>#</span><span>Name</span><span>Time</span><span>Artist</span><span>Album</span></div>
    <div className="playlist-track-scroll">
      {tracks.map((track, index) => <div
        key={`${track.uri}-${index}`}
        className={`playlist-track-row ${selected.has(index) ? 'selected' : ''} ${insertBefore === index ? 'insert-before' : ''} ${playing?.trackId === queue[index].id ? 'playing' : ''}`}
        draggable={canReorder && !mutating}
        onClick={(event) => {
          if (event.shiftKey && selectionAnchor !== undefined) {
            const next = new Set<number>()
            for (let row = Math.min(selectionAnchor, index); row <= Math.max(selectionAnchor, index); row += 1) next.add(row)
            setSelected(next)
          } else if (event.metaKey || event.ctrlKey) {
            const next = new Set(selected)
            if (!next.delete(index)) next.add(index)
            setSelected(next)
            setSelectionAnchor(index)
          } else {
            setSelected(new Set([index]))
            setSelectionAnchor(index)
          }
        }}
        onDoubleClick={() => onPlay(queue[index].id, queue)}
        onDragStart={canReorder ? (event) => {
          const rows = selected.has(index) ? [...selected].sort((left, right) => left - right) : [index]
          if (rows.some((row, offset) => row !== rows[0] + offset)) {
            event.preventDefault()
            onError('Select a contiguous block of tracks to reorder.')
            return
          }
          if (!selected.has(index)) {
            setSelected(new Set([index]))
            setSelectionAnchor(index)
          }
          setDragging({ start: rows[0], length: rows.length })
          event.dataTransfer.effectAllowed = 'move'
          event.dataTransfer.setData('text/plain', String(index))
        } : undefined}
        onDragOver={canReorder ? (event) => { event.preventDefault(); setInsertBefore(index) } : undefined}
        onDrop={canReorder ? (event) => { event.preventDefault(); void drop(index) } : undefined}
        onDragEnd={() => { setDragging(undefined); setInsertBefore(undefined) }}
      ><span>{index + 1}</span><strong title={track.name}>{track.name}</strong><time>{formatTime(track.durationSecs)}</time><span title={track.art}>{track.art}</span><span title={track.alb}>{track.alb}</span></div>)}
      {canReorder && <div className={`playlist-end-drop ${insertBefore === tracks.length ? 'insert-before' : ''}`} onDragOver={(event) => { event.preventDefault(); setInsertBefore(tracks.length) }} onDrop={(event) => { event.preventDefault(); void drop(tracks.length) }} />}
    </div>
  </div>
}

function ContextMenu({ x, y, onClose, children }: { x: number; y: number; onClose: () => void; children: React.ReactNode }) {
  useEffect(() => {
    const close = (event: PointerEvent) => { if (!(event.target as HTMLElement).closest('.context-menu')) onClose() }
    const escape = (event: KeyboardEvent) => { if (event.key === 'Escape') onClose() }
    document.addEventListener('pointerdown', close)
    document.addEventListener('keydown', escape)
    return () => {
      document.removeEventListener('pointerdown', close)
      document.removeEventListener('keydown', escape)
    }
  }, [onClose])
  return <div className="column-menu context-menu popup-context-menu" style={{ left: x, top: y }}>{children}</div>
}

function AddToPlaylist({ subject, revision, onAdd, onClose, onError }: {
  subject: PlaylistSubject
  revision: number
  onAdd: (id: string, subject: PlaylistSubject) => Promise<unknown>
  onClose: () => void
  onError: (error: string) => void
}) {
  const [playlists, setPlaylists] = useState<PlaylistListView[]>([])
  const [busy, setBusy] = useState<string>()
  const [creating, setCreating] = useState(false)
  const [name, setName] = useState('')
  useEffect(() => {
    let active = true
    invoke<PlaylistListView[]>('playlists_list', subject.kind === 'tracks' ? { uris: subject.uris } : undefined)
      .then((rows) => active && setPlaylists(rows))
      .catch((error) => active && onError(String(error)))
    return () => { active = false }
  }, [revision, subject])
  const add = async (id: string) => {
    setBusy(id)
    try { await onAdd(id, subject) }
    catch (error) { onError(String(error)) }
    finally { setBusy(undefined) }
  }
  const create = async () => {
    if (!name.trim()) return
    setBusy('new')
    try {
      const playlist = await invoke<PlaylistListView>('playlist_create', { name })
      await onAdd(playlist.id, subject)
      setName('')
      setCreating(false)
    } catch (error) {
      onError(String(error))
    } finally {
      setBusy(undefined)
    }
  }
  return <div className="modal-backdrop" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget) onClose() }}>
    <div className="playlist-popover" role="dialog" aria-modal="true" aria-labelledby="add-to-playlist-title">
      <header><h2 id="add-to-playlist-title">Add to Playlist</h2><span>{subject.label}</span></header>
      <div className="playlist-popover-list">{playlists.map((playlist) => <button key={playlist.id} disabled={!playlist.owned || busy === playlist.id} onClick={() => void add(playlist.id)}>
        <span>{playlist.contains ? '✓' : ''}</span><span>{playlist.owned ? '' : '🌐'}</span><strong>{playlist.name}</strong>{!playlist.owned && <small>{playlist.owner}</small>}
      </button>)}</div>
      <footer>{creating
        ? <input autoFocus aria-label="Playlist name" value={name} onChange={(event) => setName(event.target.value)} onKeyDown={(event) => {
          if (event.key === 'Enter') void create()
        }} />
        : <button className="new-playlist-button" onClick={() => setCreating(true)}>+ New Playlist</button>}
        <button className="done-button" onClick={onClose}>Done</button></footer>
    </div>
  </div>
}

function BrowserPane({ state, anchors, onActivate, onSelect }: {
  state: State
  anchors: { current: Partial<Record<keyof Selection, string>> }
  onActivate: (facet: keyof Selection) => void
  onSelect: (facet: keyof Selection, values: string[], anchor?: string) => void
}) {
  const sourceLabels = labels[state.source].facets
  const values = [state.view?.facets.cats ?? [], state.view?.facets.arts ?? [], state.view?.facets.albs ?? []]
  const facets: (keyof Selection)[] = ['cat', 'art', 'alb']
  return <div className="browser-pane">
    {facets.map((facet, index) => <FacetColumn key={facet} facet={facet} title={sourceLabels[index]} values={values[index]} selected={state.sel[facet]} anchor={anchors.current[facet]} onActivate={() => onActivate(facet)} onSelect={(selected, anchor) => onSelect(facet, selected, anchor)} />)}
  </div>
}

function FacetColumn({ facet, title, values, selected, anchor, onActivate, onSelect }: {
  facet: keyof Selection
  title: string
  values: string[]
  selected?: string[]
  anchor?: string
  onActivate: () => void
  onSelect: (values: string[], anchor?: string) => void
}) {
  const select = (value: string, event: React.MouseEvent) => {
    if (event.shiftKey) {
      const start = Math.max(0, values.indexOf(anchor ?? values[0]))
      const end = values.indexOf(value)
      onSelect(values.slice(Math.min(start, end), Math.max(start, end) + 1), anchor)
    } else if (event.metaKey || event.ctrlKey) {
      onSelect(selected?.includes(value) ? selected.filter((item) => item !== value) : [...(selected ?? []), value], value)
    } else {
      onSelect([value], value)
    }
  }
  return <div className="facet-column" data-facet={facet} onMouseDown={onActivate}>
    <div className="column-header">{title}</div>
    <div className="facet-list">
      <button data-row-index={0} className={!selected?.length ? 'active' : ''} onClick={() => onSelect([], undefined)}>All ({values.length} {title}s)</button>
      {values.map((value, index) => <button key={value} data-row-index={index + 1} className={selected?.includes(value) ? 'active' : ''} onClick={(event) => select(value, event)} title={value}>{value}</button>)}
    </div>
  </div>
}

function RatingStars({ rating, explicit = false, onRate }: { rating: number | null; explicit?: boolean; onRate?: (stars: number) => void }) {
  return <span className={`rating-stars ${rating ? explicit ? 'explicit' : 'inherited' : 'empty'} ${onRate ? '' : 'inert'}`} aria-label={rating ? `${rating} out of 5 stars` : 'Unrated'}>
    {[1, 2, 3, 4, 5].map((star) => <button key={star} disabled={!onRate} aria-label={`${star} stars`} onClick={(event) => { event.stopPropagation(); onRate?.(star) }}>{star <= (rating ?? 0) ? '★' : '☆'}</button>)}
  </span>
}

function AlbumRatingStrip({ album, rating, onRate }: { album: string; rating: number | null; onRate: (rating: number | null) => void }) {
  return <div className="album-rating-strip"><strong>{album}</strong><RatingStars rating={rating} explicit onRate={(stars) => onRate(stars === rating ? null : stars)} /></div>
}

function TrackList({ tracks, label, selectedIds, playing, columnOrder, hiddenColumns, empty, onActivate, onSetup, onSelect, onPlay, onRate, onInfo, onPlaylist, onReorder, onHiddenColumns }: {
  tracks: Track[]; label: (typeof labels)[Source]; selectedIds: Set<number>; playing: State['playing']
  columnOrder: ColumnKey[]; hiddenColumns: ColumnKey[]; empty: boolean; onSelect: (id: number, event: React.MouseEvent) => void; onPlay: (id: number) => void
  onRate: (id: number, stars: number) => void; onInfo: (id: number) => void; onReorder: (order: ColumnKey[]) => void
  onPlaylist: (subject: PlaylistSubject) => void
  onActivate: () => void; onSetup: () => void; onHiddenColumns: (columns: ColumnKey[]) => void
}) {
  const [dragging, setDragging] = useState<ColumnKey>()
  const [menu, setMenu] = useState<{ x: number; y: number; trackId?: number }>()
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
    <div className={`track-scroll ${empty ? 'empty-library' : ''}`}>
      {empty ? <div className="empty-prompt"><span className="empty-glyph" aria-hidden="true">♪</span><strong>Your library is empty</strong><span>Connect Spotify and sync to pull your saved music into a local overlay.</span><button onClick={onSetup}>Set Up Library…</button></div> : tracks.map((track) => {
        const isPlaying = playing?.trackId === track.id
        return <div key={track.id} data-track-id={track.id} draggable className={`track-row ${selectedIds.has(track.id) ? 'selected' : ''} ${isPlaying ? 'playing' : ''}`} style={{ gridTemplateColumns: columns }} onClick={(event) => onSelect(track.id, event)} onDoubleClick={() => onPlay(track.id)} onDragStart={(event) => {
          const dragged = selectedIds.has(track.id) ? tracks.filter((candidate) => selectedIds.has(candidate.id)) : [track]
          event.dataTransfer.effectAllowed = 'copy'
          event.dataTransfer.setData(DRAG_TYPE, JSON.stringify({ kind: 'tracks', label: dragged.length === 1 ? `Track · ${dragged[0].name}` : `${dragged.length} tracks`, uris: dragged.map((candidate) => candidate.uri) } satisfies PlaylistSubject))
        }} onContextMenu={(event) => {
          event.preventDefault()
          if (!selectedIds.has(track.id)) onSelect(track.id, event)
          const bounds = list.current?.getBoundingClientRect()
          setMenu({ x: event.clientX - (bounds?.left ?? 0), y: event.clientY - (bounds?.top ?? 0), trackId: track.id })
        }}>
          <span className="playing-marker">{isPlaying ? playing.isPlaying ? '▶' : '❚❚' : ''}</span>
          {visibleColumns.map((column) => cell(track, column))}
        </div>
      })}
    </div>
    {menu && (menu.trackId === undefined
      ? <div className="column-menu" style={{ left: menu.x, top: menu.y }}>
        {columnOrder.map((column) => <label key={column}><input type="checkbox" checked={!hiddenColumns.includes(column)} disabled={column === 'name'} onChange={(event) => onHiddenColumns(event.target.checked
          ? hiddenColumns.filter((hidden) => hidden !== column)
          : [...hiddenColumns, column])} />{headings[column]}</label>)}
      </div>
      : <div className="column-menu context-menu" style={{ left: menu.x, top: menu.y }}>
        <button onClick={() => {
          const target = tracks.find((track) => track.id === menu.trackId)
          if (!target) return
          const selected = selectedIds.has(target.id) ? tracks.filter((track) => selectedIds.has(track.id)) : [target]
          setMenu(undefined)
          onPlaylist({ kind: 'tracks', label: selected.length === 1 ? `Track · ${selected[0].name}` : `${selected.length} tracks`, uris: selected.map((track) => track.uri) })
        }}>Add to Playlist…</button>
        <button disabled>Go to Album</button>
        <button disabled>Go to Artist</button>
        <button onClick={() => { const id = menu.trackId; setMenu(undefined); if (id !== undefined) onInfo(id) }}>Get Info</button>
      </div>)}
  </div>
}

type SpotifyTab = 'all' | keyof SpotifyResults
type SpotifyNavEntry = { kind: 'artist'; id: string } | { kind: 'album'; uri: string; highlight?: string }
const SYNTHETIC_BASE = 2 ** 40

function SpotifyArtwork({ imageUrl, round = false }: { imageUrl: string | null; round?: boolean }) {
  return <span className={`spotify-artwork ${round ? 'round' : ''}`}>{imageUrl ? <img src={imageUrl} alt="" /> : <span aria-hidden="true">♪</span>}</span>
}

function SpotifyAlbumRow({ album, adding, added, onAdd, onOpen, onPlaylist, openOnClick = false, showType = false }: {
  album: SearchAlbum
  adding: boolean
  added: boolean
  onAdd: () => void
  onOpen: () => void
  onPlaylist: (subject: PlaylistSubject) => void
  openOnClick?: boolean
  showType?: boolean
}) {
  const [menu, setMenu] = useState<{ x: number; y: number }>()
  const subject: PlaylistSubject = { kind: 'album', label: `Album · ${album.name}`, albumUri: album.uri }
  return <div className="spotify-row" draggable onClick={openOnClick ? onOpen : undefined} onDoubleClick={openOnClick ? undefined : onOpen} onDragStart={(event) => {
    event.dataTransfer.effectAllowed = 'copy'
    event.dataTransfer.setData(DRAG_TYPE, JSON.stringify(subject))
  }} onContextMenu={(event) => { event.preventDefault(); setMenu({ x: event.clientX, y: event.clientY }) }}>
    <SpotifyArtwork imageUrl={album.imageUrl} />
    <span className="spotify-copy"><strong>{album.name}</strong><small>{showType ? [album.year, album.albumType].filter(Boolean).join(' · ') : <>{album.artist}{album.year && ` · ${album.year}`}</>}</small></span>
    <button className="spotify-add" disabled={adding || added} onClick={(event) => { event.stopPropagation(); onAdd() }} onDoubleClick={(event) => event.stopPropagation()}>{added ? '✓ Added' : adding ? 'Adding…' : '+ Add'}</button>
    {menu && <ContextMenu x={menu.x} y={menu.y} onClose={() => setMenu(undefined)}><button onClick={() => { setMenu(undefined); onPlaylist(subject) }}>Add to Playlist…</button></ContextMenu>}
  </div>
}

function SpotifyPageBack({ label, onBack }: { label: string; onBack: () => void }) {
  return <button className="spotify-page-back" onClick={onBack}>‹ Back to {label}</button>
}

function SpotifyAlbumPage({ entry, backLabel, adding, onBack, onArtist, onAdd, onPlay, onPlaylist, onError }: {
  entry: Extract<SpotifyNavEntry, { kind: 'album' }>
  backLabel: string
  adding: boolean
  onBack: () => void
  onArtist: (id: string) => void
  onAdd: (album: SearchAlbum) => Promise<boolean>
  onPlay: (id: number, tracks: readonly PlaybackTrack[]) => void
  onPlaylist: (subject: PlaylistSubject) => void
  onError: (error: string) => void
}) {
  const [page, setPage] = useState<AlbumPageView>()
  const [revision, setRevision] = useState(0)
  const [busy, setBusy] = useState(false)
  const [menu, setMenu] = useState<{ x: number; y: number; index: number }>()
  const highlighted = useRef<HTMLDivElement>(null)
  useEffect(() => {
    let active = true
    setPage(undefined)
    invoke<AlbumPageView>('spotify_album_page', { uri: entry.uri })
      .then((view) => active && setPage(view))
      .catch((error) => active && onError(String(error)))
    return () => { active = false }
  }, [entry.uri, revision])
  useEffect(() => {
    if (page && entry.highlight) highlighted.current?.scrollIntoView({ block: 'center' })
  }, [entry.highlight, page])
  if (!page) return <div className="spotify-page"><SpotifyPageBack label={backLabel} onBack={onBack} /><div className="spotify-stub">Loading album…</div></div>
  const tracks: PlaybackTrack[] = page.tracks.map((track, index) => ({
    id: track.trackId ?? SYNTHETIC_BASE + index,
    uri: track.uri,
    name: track.name,
    art: page.artist,
    alb: page.name,
    durationSecs: track.durationSecs,
  }))
  const refresh = () => setRevision((current) => current + 1)
  const rateAlbum = (stars: number) => invoke('set_album_rating', {
    source: 'music', art: page.artist, alb: page.name, stars: stars === page.albumRating ? null : stars,
  }).then(refresh).catch((error) => onError(String(error)))
  const rateTrack = (id: number, stars: number) => invoke('click_track_star', { id, stars }).then(refresh).catch((error) => onError(String(error)))
  const remove = async () => {
    setBusy(true)
    try {
      await invoke('remove_spotify_album', { uri: page.uri })
      refresh()
    } catch (error) {
      onError(String(error))
    } finally {
      setBusy(false)
    }
  }
  const add = async () => {
    if (await onAdd({ uri: page.uri, name: page.name, artist: page.artist, year: page.year, imageUrl: page.imageUrl, albumType: page.albumType })) refresh()
  }
  return <div className="spotify-page">
    <SpotifyPageBack label={backLabel} onBack={onBack} />
    <header className="spotify-page-header album-header">
      <div className="spotify-page-art album-art"><SpotifyArtwork imageUrl={page.imageUrl} /></div>
      <div className="spotify-page-copy">
        <div className="spotify-eyebrow">ALBUM{page.albumType.toLowerCase() !== 'album' && ` · ${page.albumType.toUpperCase()}`}</div>
        <h1>{page.name}</h1>
        <button className="spotify-link artist-link" onClick={() => onArtist(page.artistId)}>{page.artist}</button>
        <div className="spotify-page-meta"><RatingStars rating={page.albumRating} explicit onRate={page.inLibrary ? rateAlbum : undefined} /><span>{page.year && `${page.year} · `}{page.tracks.length} {page.tracks.length === 1 ? 'track' : 'tracks'} · {Math.floor(page.totalDurationSecs / 60)} min</span></div>
        <div className="spotify-page-actions">
          <button className="primary" onClick={() => onPlay(tracks[0].id, tracks)} disabled={!tracks.length}>▶ Play</button>
          {page.inLibrary
            ? <button disabled={busy} onClick={() => void remove()}>{busy ? 'Removing…' : '✓ In Library — Remove'}</button>
            : <button disabled={adding} onClick={() => void add()}>{adding ? 'Adding…' : '+ Add to Library'}</button>}
        </div>
      </div>
    </header>
    <section className="spotify-page-section album-tracks">
      {page.tracks.map((track, index) => {
        const subject: PlaylistSubject = { kind: 'tracks', label: `Track · ${track.name}`, uris: [track.uri] }
        return <div key={track.uri} ref={track.uri === entry.highlight ? highlighted : undefined} draggable className={`spotify-track-row ${track.uri === entry.highlight ? 'highlighted' : ''}`} onDoubleClick={() => onPlay(tracks[index].id, tracks)} onDragStart={(event) => {
          event.dataTransfer.effectAllowed = 'copy'
          event.dataTransfer.setData(DRAG_TYPE, JSON.stringify(subject))
        }} onContextMenu={(event) => { event.preventDefault(); setMenu({ x: event.clientX, y: event.clientY, index }) }}>
        <span>{track.trackNo ?? index + 1}</span>
        <strong>{track.name}</strong>
        <RatingStars rating={track.rating?.stars ?? null} explicit={track.rating?.explicit} onRate={track.trackId === null ? undefined : (stars) => rateTrack(track.trackId!, stars)} />
        <time>{formatTime(track.durationSecs)}</time>
        {menu?.index === index && <ContextMenu x={menu.x} y={menu.y} onClose={() => setMenu(undefined)}><button onClick={() => { setMenu(undefined); onPlaylist(subject) }}>Add to Playlist…</button></ContextMenu>}
      </div>})}
      <p className="spotify-page-hint">Double-click a track to preview. Adding the album pulls every track into your local overlay.</p>
    </section>
  </div>
}

function SpotifyArtistPage({ id, backLabel, adding, added, onBack, onAlbum, onAdd, onPlay, onPlaylist, onError }: {
  id: string
  backLabel: string
  adding: string | undefined
  added: ReadonlySet<string>
  onBack: () => void
  onAlbum: (uri: string) => void
  onAdd: (album: SearchAlbum) => Promise<boolean>
  onPlay: (id: number, tracks: readonly PlaybackTrack[]) => void
  onPlaylist: (subject: PlaylistSubject) => void
  onError: (error: string) => void
}) {
  const [page, setPage] = useState<ArtistPageView>()
  const [toggling, setToggling] = useState(false)
  useEffect(() => {
    let active = true
    setPage(undefined)
    invoke<ArtistPageView>('spotify_artist_page', { artistId: id })
      .then((view) => active && setPage(view))
      .catch((error) => active && onError(String(error)))
    return () => { active = false }
  }, [id])
  if (!page) return <div className="spotify-page"><SpotifyPageBack label={backLabel} onBack={onBack} /><div className="spotify-stub">Loading artist…</div></div>
  const tracks: PlaybackTrack[] = page.topTracks.map((track, index) => ({
    id: SYNTHETIC_BASE + index,
    uri: track.uri,
    name: track.name,
    art: page.name,
    alb: track.alb,
    durationSecs: track.durationSecs,
  }))
  const toggleFollow = async () => {
    const following = !page.following
    setPage({ ...page, following })
    setToggling(true)
    try {
      await invoke('spotify_follow_artist', { artistId: page.id, follow: following })
    } catch (error) {
      setPage((current) => current ? { ...current, following: !following } : current)
      onError(String(error))
    } finally {
      setToggling(false)
    }
  }
  return <div className="spotify-page">
    <SpotifyPageBack label={backLabel} onBack={onBack} />
    <header className="spotify-page-header artist-header">
      <div className="spotify-page-art artist-art"><SpotifyArtwork imageUrl={page.imageUrl} round /></div>
      <div className="spotify-page-copy">
        <div className="spotify-eyebrow">ARTIST</div>
        <h1>{page.name}</h1>
        <div className="spotify-page-meta">{page.descriptor} · {page.albums.length} {page.albums.length === 1 ? 'album' : 'albums'}</div>
        <div className="spotify-page-actions">
          <button className="primary" onClick={() => onPlay(tracks[0].id, tracks)} disabled={!tracks.length}>▶ Play</button>
          <button disabled={toggling} onClick={() => void toggleFollow()}>{page.following ? '✓ Following' : '+ Follow'}</button>
        </div>
      </div>
    </header>
    <section className="spotify-page-section">
      <h2>Discography</h2>
      {page.albums.map((album) => <SpotifyAlbumRow key={album.uri} album={album} adding={adding === album.uri} added={added.has(album.uri)} onAdd={() => { void onAdd(album) }} onOpen={() => onAlbum(album.uri)} onPlaylist={onPlaylist} openOnClick showType />)}
      {!page.albums.length && <p>No albums found.</p>}
    </section>
    <section className="spotify-page-section top-tracks">
      <h2>Top Tracks</h2>
      {page.topTracks.map((track, index) => <div className="spotify-top-track-row" key={track.uri} onDoubleClick={() => onPlay(tracks[index].id, tracks)}>
        <span>{index + 1}</span><strong>{track.name}</strong><span>{track.alb}</span><time>{formatTime(track.durationSecs)}</time>
      </div>)}
      {!page.topTracks.length && <p>No top tracks found.</p>}
    </section>
  </div>
}

function SpotifySearch({ query, searching, results, onAdd, onPlay, onPlaylist, onError }: {
  query: string
  searching: boolean
  results: SpotifyResults | null
  onAdd: (album: SpotifyResults['albums'][number]) => Promise<unknown>
  onPlay: (id: number, tracks: readonly PlaybackTrack[]) => void
  onPlaylist: (subject: PlaylistSubject) => void
  onError: (error: string) => void
}) {
  const [tab, setTab] = useState<SpotifyTab>('all')
  const [adding, setAdding] = useState<string>()
  const [added, setAdded] = useState<ReadonlySet<string>>(new Set())
  const [nav, setNav] = useState<SpotifyNavEntry[]>([])
  const [menu, setMenu] = useState<{ x: number; y: number; track: SpotifyResults['tracks'][number] }>()
  useEffect(() => {
    setTab('all')
    setAdded(new Set())
    setNav([])
  }, [query])
  const add = async (album: SpotifyResults['albums'][number]) => {
    setAdding(album.uri)
    try {
      await onAdd(album)
      setAdded((previous) => new Set(previous).add(album.uri))
      return true
    } catch {
      return false
    } finally {
      setAdding(undefined)
    }
  }
  const pushAlbum = (uri: string, highlight?: string) => setNav((current) => [...current, { kind: 'album', uri, highlight }])
  const top = nav[nav.length - 1]
  const below = nav[nav.length - 2]
  const backLabel = below?.kind ?? 'results'
  if (searching) return <div className="spotify-stub">Searching Spotify…</div>
  if (top?.kind === 'album') return <SpotifyAlbumPage entry={top} backLabel={backLabel} adding={adding === top.uri} onBack={() => setNav((current) => current.slice(0, -1))} onArtist={(id) => setNav((current) => [...current, { kind: 'artist', id }])} onAdd={add} onPlay={onPlay} onPlaylist={onPlaylist} onError={onError} />
  if (top?.kind === 'artist') return <SpotifyArtistPage id={top.id} backLabel={backLabel} adding={adding} added={added} onBack={() => setNav((current) => current.slice(0, -1))} onAlbum={pushAlbum} onAdd={add} onPlay={onPlay} onPlaylist={onPlaylist} onError={onError} />
  const counts = {
    artists: results?.artists.length ?? 0,
    albums: results?.albums.length ?? 0,
    tracks: results?.tracks.length ?? 0,
  }
  const tabs: { key: SpotifyTab; label: string; count: number }[] = [
    { key: 'all', label: 'All', count: counts.artists + counts.albums + counts.tracks },
    { key: 'artists', label: 'Artists', count: counts.artists },
    { key: 'albums', label: 'Albums', count: counts.albums },
    { key: 'tracks', label: 'Tracks', count: counts.tracks },
  ]
  return <div className="spotify-results-view">
    <div className="spotify-tabs" role="tablist" aria-label="Spotify result filters">
      {tabs.map((item) => <button key={item.key} role="tab" aria-selected={tab === item.key} className={tab === item.key ? 'active' : ''} onClick={() => setTab(item.key)}>{item.label} ({item.count})</button>)}
      <span>Spotify · &quot;{query}&quot;</span>
    </div>
    <div className="spotify-results">
      {(tab === 'all' || tab === 'artists') && <section>
        {tab === 'all' && <h2>Artists</h2>}
        {results?.artists.map((artist) => <div className="spotify-row" key={artist.id}>
          <SpotifyArtwork imageUrl={artist.imageUrl} round />
          <span className="spotify-copy"><strong>{artist.name}</strong><small>{artist.descriptor}</small></span>
          <button className="spotify-link" onClick={() => setNav((current) => [...current, { kind: 'artist', id: artist.id }])}>View albums ›</button>
        </div>)}
        {!results?.artists.length && <p>No artists found.</p>}
      </section>}
      {(tab === 'all' || tab === 'albums') && <section>
        {tab === 'all' && <h2>Albums</h2>}
        {results?.albums.map((album) => <SpotifyAlbumRow key={album.uri} album={album} adding={adding === album.uri} added={added.has(album.uri)} onAdd={() => { void add(album) }} onOpen={() => pushAlbum(album.uri)} onPlaylist={onPlaylist} />)}
        {!results?.albums.length && <p>No albums found.</p>}
      </section>}
      {(tab === 'all' || tab === 'tracks') && <section>
        {tab === 'all' && <h2>Tracks</h2>}
        {results?.tracks.map((track) => <div className="spotify-row" key={track.uri} onContextMenu={(event) => { event.preventDefault(); setMenu({ x: event.clientX, y: event.clientY, track }) }}>
          <SpotifyArtwork imageUrl={track.imageUrl} />
          <span className="spotify-copy"><strong>{track.name}</strong><small>{track.artist} · {track.alb}</small></span>
          <time>{Math.floor(track.durationSecs / 60)}:{String(Math.floor(track.durationSecs % 60)).padStart(2, '0')}</time>
        </div>)}
        {!results?.tracks.length && <p>No tracks found.</p>}
      </section>}
    </div>
    {menu && <ContextMenu x={menu.x} y={menu.y} onClose={() => setMenu(undefined)}>
      <button onClick={() => {
        const track = menu.track
        setMenu(undefined)
        onPlaylist({ kind: 'tracks', label: `Track · ${track.name}`, uris: [track.uri] })
      }}>Add to Playlist…</button>
      <button disabled={!menu.track.albumUri} onClick={() => { const track = menu.track; setMenu(undefined); if (track.albumUri) pushAlbum(track.albumUri, track.uri) }}>Go to Album</button>
    </ContextMenu>}
  </div>
}

function AutocompleteInput({ suggestions, value, onValue, placeholder }: {
  suggestions: string[]
  value: string
  onValue: (value: string) => void
  placeholder?: string
}) {
  return <input value={value} placeholder={placeholder} onChange={(event) => {
    const input = event.target
    const typed = input.value
    const insertion = (event.nativeEvent as InputEvent).inputType === 'insertText' && input.selectionStart === typed.length
    const suggestion = insertion && suggestions.find((candidate) => candidate.length > typed.length && candidate.toLowerCase().startsWith(typed.toLowerCase()))
    if (suggestion) {
      input.value = suggestion
      input.setSelectionRange(typed.length, suggestion.length)
    }
    onValue(suggestion || typed)
  }} />
}

function GetInfo({ track, onCancel, onSaved, onError }: { track: TrackInfo; onCancel: () => void; onSaved: () => void; onError: (error: string) => void }) {
  const [draft, setDraft] = useState({ name: track.name, art: track.art, alb: track.alb, cat: track.cat === 'Uncategorized' ? '' : track.cat })
  const [suggestions, setSuggestions] = useState<MetadataValues>({ arts: [], albs: [], cats: [] })
  const [rating, setRating] = useState(track.rating)
  const dialog = useRef<HTMLDivElement>(null)
  useEffect(() => {
    dialog.current?.focus()
    invoke<MetadataValues>('metadata_values').then(setSuggestions).catch((error) => onError(String(error)))
  }, [])
  const genres = useMemo(() => [...new Map([...suggestions.cats, ...track.genres].filter((genre) => genre && genre !== 'Uncategorized').map((genre) => [genre.toLowerCase(), genre] as const)).values()]
    .sort((left, right) => left.toLowerCase().localeCompare(right.toLowerCase())), [suggestions.cats, track.genres])
  const rate = (stars: number) => setRating((current) => current?.explicit && current.stars === stars
    ? track.inheritedRating === null ? null : { stars: track.inheritedRating, explicit: false }
    : { stars, explicit: true })
  const save = async () => {
    try {
      const ratingChange = { stars: rating?.explicit ? rating.stars : null }
      const edit = Object.fromEntries(Object.entries(draft).filter(([, value]) => value.trim() !== ''))
      await invoke('edit_track', { id: track.id, edit: { ...edit, ratingChange } })
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
      <label>Artist<AutocompleteInput suggestions={suggestions.arts} value={draft.art} onValue={(art) => setDraft({ ...draft, art })} /></label>
      <label>Album<AutocompleteInput suggestions={suggestions.albs} value={draft.alb} onValue={(alb) => setDraft({ ...draft, alb })} /></label>
      <label>Genre<AutocompleteInput suggestions={genres} value={draft.cat} onValue={(cat) => setDraft({ ...draft, cat })} placeholder={track.cat === 'Uncategorized' ? 'Uncategorized' : undefined} /></label>
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
  const [suggestions, setSuggestions] = useState<MetadataValues>({ arts: [], albs: [], cats: [] })
  const [rating, setRating] = useState<number | null | undefined>(undefined)
  const dialog = useRef<HTMLDivElement>(null)
  useEffect(() => {
    dialog.current?.focus()
    invoke<MetadataValues>('metadata_values').then(setSuggestions).catch((error) => onError(String(error)))
  }, [])
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
  const field = (key: Field, values: string[]) => ({
    suggestions: values,
    value: draft[key] ?? '',
    placeholder: placeholder(key),
    onValue: (value: string) => setDraft((current) => {
      const next = { ...current }
      if (value.trim()) next[key] = value
      else delete next[key]
      return next
    }),
  })
  return <div className="modal-backdrop" role="presentation">
    <div className="get-info" role="dialog" aria-modal="true" aria-labelledby="multiple-item-information-title" tabIndex={-1} ref={dialog}>
      <h2 id="multiple-item-information-title">Editing {tracks.length} items</h2>
      <label>Artist<AutocompleteInput {...field('art', suggestions.arts)} /></label>
      <label>Album<AutocompleteInput {...field('alb', suggestions.albs)} /></label>
      <label>Genre<AutocompleteInput {...field('cat', suggestions.cats)} /></label>
      <div className="info-rating bulk-rating"><span>Rating</span><RatingStars rating={rating ?? null} explicit={rating !== undefined && rating !== null} onRate={setRating} /><button className={rating === undefined ? 'active' : ''} onClick={() => setRating(undefined)}>No Change</button><button className={rating === null ? 'active' : ''} onClick={() => setRating(null)}>Clear</button></div>
      <div className="modal-actions"><button onClick={onCancel}>Cancel</button><button className="primary" onClick={() => void save()}>Save Overlay</button></div>
    </div>
  </div>
}

function SetupLibrary({ settings, connected, onCancel, onConnect, onSync }: {
  settings: Settings
  connected: boolean
  onCancel: () => void
  onConnect: (clientId: string) => void
  onSync: (clientId: string) => void
}) {
  const [clientId, setClientId] = useState(settings.spotifyClientId)
  const [webApi, setWebApi] = useState(true)
  const dialog = useRef<HTMLDivElement>(null)
  useEffect(() => { dialog.current?.focus() }, [])
  const trimmedClientId = clientId.trim()
  return <div className="modal-backdrop" role="presentation">
    <div className="get-info preferences setup-library" role="dialog" aria-modal="true" aria-labelledby="setup-library-title" tabIndex={-1} ref={dialog}>
      <h2 id="setup-library-title">Set Up Your Library</h2>
      <div className="setup-content">
        <p>Retune reads your Spotify library through the Web API and builds a local overlay. Confirm three things, then sync.</p>
        <div className="setup-step"><span className="step-number">1</span><label><strong>Spotify app Client ID</strong><input value={clientId} onChange={(event) => setClientId(event.target.value)} /><small>Create one at developer.spotify.com → Dashboard → your app.</small></label></div>
        <div className="setup-step"><span className="step-number">2</span><div><label className="setup-check"><input type="checkbox" checked={webApi} onChange={(event) => setWebApi(event.target.checked)} /><strong>Web API enabled</strong></label><small>The app must have the Web API scope turned on in its dashboard settings.</small></div></div>
        <div className="setup-step"><span className="step-number">3</span><div><strong>Spotify connection</strong><div className={`setup-status ${connected ? 'connected' : ''}`}><span className="connection-dot" /><span>{connected ? 'Connected' : 'Not connected'}</span>{connected ? <span className="detected">✓ auto-detected</span> : <button onClick={() => onConnect(trimmedClientId)}>Connect…</button>}</div></div></div>
      </div>
      <div className="modal-actions"><button onClick={onCancel}>Cancel</button><button className="primary" disabled={!trimmedClientId || !webApi || !connected} onClick={() => onSync(trimmedClientId)}>Sync</button></div>
    </div>
  </div>
}

function Preferences({ settings, onZoom, onCancel, onSave }: {
  settings: Settings
  onZoom: (zoom: number) => void
  onCancel: () => void
  onSave: (theme: Theme, autoAdd: boolean, autoConnect: boolean, clientId: string, playbackBackend: PlaybackBackend, streamingBitrate: number, normalizeVolume: boolean, gapless: boolean) => void
}) {
  type PreferenceTab = 'appearance' | 'library' | 'audio'
  const [tab, setTab] = useState<PreferenceTab>('appearance')
  const [theme, setTheme] = useState(settings.theme)
  const [autoAdd, setAutoAdd] = useState(settings.autoAddSpotifyLibrary)
  const [autoConnect, setAutoConnect] = useState(settings.autoConnect)
  const [clientId, setClientId] = useState(settings.spotifyClientId)
  const [playbackBackend, setPlaybackBackend] = useState(settings.playbackBackend)
  const [streamingBitrate, setStreamingBitrate] = useState(settings.streamingBitrate)
  const [normalizeVolume, setNormalizeVolume] = useState(settings.normalizeVolume)
  const [gapless, setGapless] = useState(settings.gapless)
  const dialog = useRef<HTMLDivElement>(null)
  useEffect(() => { dialog.current?.focus() }, [])
  const qualityIndex = Math.max(0, streamingQualities.findIndex(([, bitrate]) => bitrate === streamingBitrate))
  const tabs: [PreferenceTab, string, string][] = [
    ['appearance', '◐', 'Appearance'],
    ['library', '♪', 'Library'],
    ['audio', '🔊', 'Audio'],
  ]
  const themeOptions: [Theme, string, string][] = [
    ['system', 'System', 'Follow the OS appearance, switching automatically.'],
    ['light', 'Light', 'Always use the light theme.'],
    ['dark', 'Dark', 'Always use the dark theme.'],
  ]
  return <div className="modal-backdrop" role="presentation">
    <div className="get-info preferences" role="dialog" aria-modal="true" aria-labelledby="preferences-title" tabIndex={-1} ref={dialog}>
      <h2 id="preferences-title">Preferences</h2>
      <div className="preference-tabs" role="tablist">
        {tabs.map(([value, glyph, label]) => <button key={value} id={`preferences-${value}-tab`} className={tab === value ? 'active' : ''} role="tab" aria-selected={tab === value} aria-controls={`preferences-${value}`} onClick={() => setTab(value)}><span aria-hidden="true">{glyph}</span>{label}</button>)}
      </div>
      <div className="preference-content" id={`preferences-${tab}`} role="tabpanel" aria-labelledby={`preferences-${tab}-tab`}>
        {tab === 'appearance' && <>
          <div className="preference-section-label">Theme</div>
          <div className="preference-options">
            {themeOptions.map(([value, label, help]) => <label className="preference-radio" key={value}><input type="radio" name="theme" value={value} checked={theme === value} onChange={() => setTheme(value)} /><span><strong>{label}</strong><small>{help}</small></span></label>)}
          </div>
          <div className="preference-section-label preference-size-label">Size</div>
          <input
            className="preference-range"
            type="range"
            aria-label="Interface size"
            min={ZOOM_MIN}
            max={ZOOM_MAX}
            step="0.1"
            value={settings.zoom}
            style={{ '--range-progress': `${(settings.zoom - ZOOM_MIN) / (ZOOM_MAX - ZOOM_MIN) * 100}%` } as React.CSSProperties}
            onChange={(event) => onZoom(Number(event.target.value))}
          />
          <div className="range-labels range-edges"><span>Smaller</span><span>Larger</span></div>
        </>}
        {tab === 'library' && <>
          <label className="client-id-field"><strong>Spotify Client ID</strong><input value={clientId} onChange={(event) => setClientId(event.target.value)} placeholder="From developer.spotify.com" /></label>
          <label className="preference-switch"><input type="checkbox" checked={autoAdd} onChange={(event) => setAutoAdd(event.target.checked)} /><span className="switch-control" aria-hidden="true" /><span><strong>Automatically add my entire Spotify library</strong><small>Everything you save on Spotify appears here automatically.</small></span></label>
          <label className="preference-switch"><input type="checkbox" checked={autoConnect} onChange={(event) => setAutoConnect(event.target.checked)} /><span className="switch-control" aria-hidden="true" /><span><strong>Connect to Spotify automatically at launch</strong><small>Keep pulling in music you add on Spotify each time Retune starts.</small></span></label>
        </>}
        {tab === 'audio' && <>
          <div className="quality-heading"><strong>Streaming quality</strong><span>{streamingQualities[qualityIndex][0]} – {streamingQualities[qualityIndex][1]} kbps</span></div>
          <input
            className="preference-range"
            type="range"
            aria-label="Streaming quality"
            min="0"
            max={streamingQualities.length - 1}
            value={qualityIndex}
            style={{ '--range-progress': `${qualityIndex / (streamingQualities.length - 1) * 100}%` } as React.CSSProperties}
            onChange={(event) => setStreamingBitrate(streamingQualities[Number(event.target.value)][1])}
          />
          <div className="range-labels quality-labels">{streamingQualities.map(([label]) => <span key={label}>{label}</span>)}</div>
          <div className="preference-section-label playback-label">Playback</div>
          <div className="preference-options">
            <label className="preference-radio"><input type="radio" name="playback-backend" checked={playbackBackend === 'connect'} onChange={() => setPlaybackBackend('connect')} /><span><strong>Spotify app (Connect)</strong><small>Route playback through the running Spotify desktop app.</small></span></label>
            <label className="preference-radio"><input type="radio" name="playback-backend" checked={playbackBackend === 'local'} onChange={() => setPlaybackBackend('local')} /><span><strong>Built-in (librespot)</strong><small>Play directly inside Retune — no Spotify app window needed.</small></span></label>
          </div>
          <label className="preference-switch compact"><input type="checkbox" checked={normalizeVolume} onChange={(event) => setNormalizeVolume(event.target.checked)} /><span className="switch-control" aria-hidden="true" /><span><strong>Normalize volume across tracks</strong></span></label>
          <label className="preference-switch compact"><input type="checkbox" checked={gapless} onChange={(event) => setGapless(event.target.checked)} /><span className="switch-control" aria-hidden="true" /><span><strong>Gapless album playback</strong></span></label>
        </>}
      </div>
      <div className="modal-actions"><button onClick={onCancel}>Cancel</button><button className="primary" onClick={() => onSave(theme, autoAdd, autoConnect, clientId.trim(), playbackBackend, streamingBitrate, normalizeVolume, gapless)}>Save</button></div>
    </div>
  </div>
}

function StatusBar({ view, unit, syncPhase, syncProgress, empty }: { view: BrowseView | null; unit: string; syncPhase?: string; syncProgress?: { tracks: number; fraction: number }; empty: boolean }) {
  const total = view?.counts.totalSecs ?? 0
  const hours = Math.floor(total / 3600)
  const minutes = Math.floor((total % 3600) / 60)
  const count = view?.counts.tracks ?? 0
  return <footer className="status-bar"><button aria-label="Add">+</button>{syncProgress
    ? <span className="sync-status"><span>⟳ Syncing from Spotify…</span><progress className="sync-meter" max={1} value={syncProgress.fraction} /><span>{syncProgress.tracks} tracks synced</span></span>
    : <span>{syncPhase ?? (empty ? 'No library — set up to begin' : `${count} ${count === 1 ? unit : `${unit}s`}, ${hours}:${String(minutes).padStart(2, '0')} hours`)}</span>}</footer>
}

export default App
