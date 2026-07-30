import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { Fragment, useCallback, useEffect, useMemo, useReducer, useRef, useState } from 'react'
import './App.css'
import { COLUMN_SPECS, compareTracks, DRAG_LOCAL_TYPE, DRAG_TYPE, facetLabel, formatTime, hasLocalTracks, insertionIndexAtY, isCurrentTrack, labels, moveToIndex, nextNativeDragActive, normalizeZoom, parseDragRange, playbackQueue, playlistRows, SYNTHETIC_BASE, trackColumnHeadings, trackGridColumns } from './ui.ts'
import { GetInfo, MultipleItemInformation, Preferences, SetupLibrary } from './dialogViews.tsx'
import { AlbumRatingStrip, BrowserPane, TrackCell, TrackList } from './libraryViews.tsx'
import { SpotifySearch } from './spotifyViews.tsx'
import type { ActivePane, BrowseView, BrowserPanes, ColumnKey, ConnectionState, ImportSummary, InfoDialog, PlaybackTrack, PlayerState, Playing, PlaylistListView, PlaylistSubject, PlaylistTrack, RepeatMode, Selection, Settings, Source, SpotifyNavEntry, SpotifyResults, Theme, Track, TrackInfo } from './types.ts'
import { CheckboxMenu, ContextMenu, ModalDialog } from './viewShared.tsx'

const PLAYLIST_TRACK_DRAG_TYPE = 'application/x-retune-playlist-track'
const LOCAL_PLAYLIST_HINT = "Selection includes local files — Spotify playlists can't contain them."

const emptyTracks: Track[] = []
const ZOOM_MIN = 0.7
const ZOOM_MAX = 1.8
// Settings persisted by set_repeat / set_audio_settings — excluded from the
// generic settings-save effect (set_settings also switches playback backends).
const EXCLUDED = ['repeat', 'streamingBitrate', 'normalizeVolume', 'gapless'] as const

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
  connection: ConnectionState
  spotifyResults: SpotifyResults | null
  spotifySearching: boolean
  spotifyNavigation?: SpotifyNavEntry
  selectedPlaylist?: string
  playlistRevision: number
  syncPhase?: string
  syncProgress?: { tracks: number; fraction: number }
  importStatus?: string
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
  | { type: 'browserPanes'; browserPanes: BrowserPanes }
  | { type: 'systemTheme'; dark: boolean }
  | { type: 'refresh' }
  | { type: 'notice'; notice?: string }
  | { type: 'info'; info?: InfoDialog }
  | { type: 'preferences'; open: boolean }
  | { type: 'setup'; open: boolean }
  | { type: 'connection'; connection: ConnectionState }
  | { type: 'spotifyResults'; results: SpotifyResults | null }
  | { type: 'spotifySearching'; searching: boolean }
  | { type: 'spotifyNavigate'; entry: SpotifyNavEntry }
  | { type: 'syncPhase'; phase?: string }
  | { type: 'syncProgress'; progress: { tracks: number; fraction: number } }
  | { type: 'importStarted' }
  | { type: 'importComplete'; summary: ImportSummary }
  | { type: 'importFailed' }
  | { type: 'clearImportStatus' }
  | { type: 'playlistsRefresh' }

const defaultSettings: Settings = {
  theme: 'system',
  zoom: 1,
  zebra: true,
  plCollapsed: false,
  browserVisible: true,
  browserPanes: { cat: true, art: true, alb: true },
  // Mirrors Settings::default() in src-tauri/src/store.rs — keep in sync
  columnOrder: ['name', 'artist', 'album', 'disc', 'track', 'time', 'rating', 'genre', 'plays', 'kind', 'bitrate', 'lastPlayed', 'added', 'releaseDate'],
  columnWidths: {},
  hiddenColumns: ['disc', 'kind', 'bitrate', 'lastPlayed', 'added', 'releaseDate'],
  playlistHiddenColumns: {},
  sortColumn: null,
  sortDesc: false,
  autoAddSpotifyLibrary: true,
  autoConnect: true,
  spotifyClientId: '',
  spotifySyncCompleted: false,
  playbackBackend: 'local',
  repeat: 'off',
  shuffle: false,
  volume: 62,
  streamingBitrate: 320,
  normalizeVolume: false,
  gapless: true,
  playThresholdPercent: 100,
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
  connection: { connected: false, needs_reauth: false },
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
      return { ...state, source: action.source, sel: {}, query: '', spotifyNavigation: undefined, selectedPlaylist: undefined, selectedTrackIds: new Set(), selectionAnchor: undefined }
    case 'playlist':
      return { ...state, selectedPlaylist: action.id, spotifyNavigation: undefined, sel: {}, selectedTrackIds: new Set(), selectionAnchor: undefined }
    case 'select': {
      const sel = action.facet === 'cat'
        ? { cat: action.values }
        : action.facet === 'art'
          ? { cat: state.sel.cat, art: action.values }
          : { ...state.sel, alb: action.values }
      return { ...state, sel, selectedTrackIds: new Set(), selectionAnchor: undefined }
    }
    case 'query':
      return { ...state, query: action.query, spotifyNavigation: undefined, selectedTrackIds: new Set(), selectionAnchor: undefined }
    case 'scope':
      return { ...state, scope: action.scope, spotifyNavigation: undefined }
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
          shuffle: state.settings.shuffle, simulated: true,
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
    case 'browserPanes': {
      const sel = { ...state.sel }
      for (const facet of ['cat', 'art', 'alb'] as const) if (!action.browserPanes[facet]) delete sel[facet]
      return { ...state, sel, settings: { ...state.settings, browserPanes: action.browserPanes } }
    }
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
      return { ...state, connection: action.connection }
    case 'spotifyResults':
      return { ...state, spotifyResults: action.results, spotifySearching: false }
    case 'spotifySearching':
      return { ...state, spotifySearching: action.searching }
    case 'spotifyNavigate':
      return { ...state, scope: 'spotify', query: '', spotifyNavigation: action.entry, selectedPlaylist: undefined }
    case 'syncPhase':
      return { ...state, syncPhase: action.phase, syncProgress: action.phase ? state.syncProgress : undefined }
    case 'syncProgress':
      return { ...state, syncProgress: action.progress.fraction < 1 ? action.progress : undefined }
    case 'importStarted':
      return { ...state, importStatus: 'Importing local files…' }
    case 'importComplete':
      return { ...state, importStatus: `Imported ${action.summary.imported} tracks (${action.summary.duplicates} duplicates skipped, ${action.summary.failed.length} failed)` }
    case 'importFailed':
      return { ...state, importStatus: undefined }
    case 'clearImportStatus':
      return { ...state, importStatus: undefined }
    case 'playlistsRefresh':
      return { ...state, playlistRevision: state.playlistRevision + 1 }
  }
}


function useTauriEvent<T = unknown>(event: string, handler: (payload: T) => void) {
  const ref = useRef(handler)
  ref.current = handler
  useEffect(() => {
    const sub = listen<T>(event, ({ payload }) => ref.current(payload))
    return () => { void sub.then((stop) => stop()) }
  }, [event])
}

function usePlayer(connected: boolean, playing: Playing | null, dispatch: React.Dispatch<Action>) {
  const queue = useRef<readonly PlaybackTrack[]>(emptyTracks)
  const pendingPlay = useRef<{ id: number; tracks: readonly PlaybackTrack[] } | null>(null)
  const playingRef = useRef(playing)
  const volumeTimer = useRef<number>(undefined)
  playingRef.current = playing

  useTauriEvent<PlayerState>('player-state', (player) => {
    dispatch({ type: 'playerState', player, queue: queue.current })
  })

  const run = useCallback((command: string, args?: Record<string, unknown>) => {
    invoke(command, args).catch((error) => dispatch({ type: 'error', error: String(error) }))
  }, [dispatch])

  const liveBackend = useCallback(() => {
    const current = playingRef.current
    return !current?.simulated && (connected || current?.uri?.startsWith('file:'))
  }, [connected])

  const start = useCallback((id: number, tracks: readonly PlaybackTrack[]) => {
    const playable = playbackQueue(tracks, id)
    const target = playable.find((track) => track.id === id)
    if (target?.uri.startsWith('spotify:') && !connected) {
      // Kick off the OAuth flow instead of erroring; the pending play fires
      // once connection-changed reports connected.
      pendingPlay.current = { id, tracks: playable }
      run('connect_spotify')
      return
    }
    if (target?.uri.startsWith('file:') || target?.uri.startsWith('spotify:')) {
      queue.current = playable
      run('play_tracks', { snapshot: playable, startIndex: playable.findIndex((track) => track.id === id) })
      return
    }
    dispatch({ type: 'play', id, queue: playable })
  }, [connected, dispatch, run])

  useEffect(() => {
    if (!connected || !pendingPlay.current) return
    const { id, tracks } = pendingPlay.current
    pendingPlay.current = null
    queue.current = tracks
    run('play_tracks', { snapshot: tracks, startIndex: tracks.findIndex((track) => track.id === id) })
  }, [connected, run])

  const toggle = useCallback(() => {
    if (liveBackend()) {
      if (playingRef.current && !playingRef.current.external) run('player_toggle')
    }
    else dispatch({ type: 'togglePlay' })
  }, [dispatch, liveBackend, run])

  const step = useCallback((direction: number) => {
    const current = playingRef.current
    if (liveBackend()) {
      if (current && !current.external) run(direction < 0 ? 'player_prev' : 'player_next')
      return
    }
    if (!current?.queue.length || current.trackId === null) return
    const index = current.queue.findIndex((track) => track.id === current.trackId)
    const next = current.queue[(index + direction + current.queue.length) % current.queue.length]
    dispatch({ type: 'step', id: next.id })
  }, [dispatch, liveBackend, run])

  const setVolume = useCallback((volume: number) => {
    if (!liveBackend()) return
    window.clearTimeout(volumeTimer.current)
    volumeTimer.current = window.setTimeout(() => run('player_set_volume', { volume }), 150)
  }, [liveBackend, run])

  const seek = useCallback((seconds: number) => {
    if (liveBackend()) {
      if (playingRef.current && !playingRef.current.external) run('player_seek', { seconds })
      return
    }
    dispatch({ type: 'seek', elapsed: seconds })
  }, [dispatch, liveBackend, run])

  useEffect(() => () => window.clearTimeout(volumeTimer.current), [])

  return useMemo(() => ({ start, toggle, step, setVolume, seek }), [seek, setVolume, start, step, toggle])
}

function App() {
  const [state, dispatch] = useReducer(reducer, initialState)
  const fail = useCallback((error: unknown) => dispatch({ type: 'error', error: String(error) }), [])
  const [nativeDragActive, setNativeDragActive] = useState(false)
  const [activePane, setActivePane] = useState<ActivePane>('track')
  const [playlists, setPlaylists] = useState<PlaylistListView[]>()
  const [playlistSubject, setPlaylistSubject] = useState<PlaylistSubject>()
  const search = useRef<HTMLInputElement>(null)
  const preferenceZoom = useRef(defaultSettings.zoom)
  const skipSettingsSave = useRef(false)
  const facetAnchors = useRef<Partial<Record<keyof Selection, string>>>({})
  const typeahead = useRef({ buffer: '', timer: 0 })
  const view = state.view
  const tracks = view?.tracks ?? emptyTracks
  const displayedTracks = useMemo(() => state.settings.sortColumn
    ? [...tracks].sort((left, right) => compareTracks(left, right, state.settings.sortColumn!, state.settings.sortDesc))
    : tracks, [state.settings.sortColumn, state.settings.sortDesc, tracks])
  const selectedTracks = displayedTracks.filter((track) => state.selectedTrackIds.has(track.id))
  const spotifySearchActive = state.scope === 'spotify' && Boolean(state.query.trim() || state.spotifyNavigation)
  const tracklistVisible = !spotifySearchActive && !state.selectedPlaylist
  const libraryEmpty = view?.counts.perSource[state.source] === 0 && !state.syncPhase && !state.syncProgress
  const playbackTracks = state.playing?.queue ?? emptyTracks
  const player = usePlayer(state.connection.connected, state.playing, dispatch)
  const addToPlaylist = useCallback((id: string, subject: PlaylistSubject) => subject.kind === 'album'
    ? invoke('playlist_add_album', { id, albumUri: subject.albumUri, albumLabel: subject.label })
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
  const setBrowserPanes = useCallback((browserPanes: BrowserPanes) => {
    for (const facet of ['cat', 'art', 'alb'] as const) {
      if (!browserPanes[facet]) delete facetAnchors.current[facet]
    }
    dispatch({ type: 'browserPanes', browserPanes })
  }, [])
  const toggleBrowser = useCallback(() => {
    dispatch({ type: 'settings', settings: { browserVisible: !state.settings.browserVisible } })
  }, [state.settings.browserVisible])
  const toggleBrowserPane = useCallback((facet: keyof BrowserPanes) => {
    const browserPanes = { ...state.settings.browserPanes, [facet]: !state.settings.browserPanes[facet] }
    setBrowserPanes(browserPanes)
    if (!browserPanes[facet]) setActivePane('track')
  }, [setBrowserPanes, state.settings.browserPanes])
  const openInfo = (id?: number) => {
    if (selectedTracks.length > 1) {
      dispatch({ type: 'info', info: { kind: 'multiple', tracks: selectedTracks } })
      return
    }
    const target = id ?? selectedTracks[0]?.id
    if (target === undefined) return
    invoke<TrackInfo>('get_track', { id: target })
      .then((track) => dispatch({ type: 'info', info: { kind: 'single', track } }))
      .catch(fail)
  }

  useEffect(() => {
    let active = true
    invoke<BrowseView>('browse', {
      source: state.source,
      sel: { cat: state.sel.cat ?? [], art: state.sel.art ?? [], alb: state.sel.alb ?? [] },
      query: state.scope === 'library' && state.query.trim() ? state.query : undefined,
    }).then((next) => active && dispatch({ type: 'view', view: next }))
      .catch((error) => active && fail(error))
    return () => { active = false }
  }, [state.source, state.sel, state.query, state.scope, state.revision, fail])

  useEffect(() => {
    let active = true
    invoke<PlaylistListView[]>('playlists_list')
      .then((rows) => active && setPlaylists(rows))
      .catch((error) => active && fail(error))
    return () => { active = false }
  }, [state.playlistRevision, fail])

  useEffect(() => {
    if (playlists && state.selectedPlaylist && !playlists.some((playlist) => playlist.id === state.selectedPlaylist)) {
      dispatch({ type: 'source', source: 'music' })
    }
  }, [playlists, state.selectedPlaylist])

  useEffect(() => {
    invoke<Settings>('get_settings')
      .then((settings) => dispatch({ type: 'hydrateSettings', settings }))
      .catch(fail)
    invoke<string | null>('startup_notice')
      .then((notice) => dispatch({ type: 'notice', notice: notice ?? undefined }))
      .catch(fail)
    invoke<ConnectionState>('connection_state')
      .then((connection) => dispatch({ type: 'connection', connection }))
      .catch(fail)
  }, [fail])

  const saveKey = useMemo(
    () => JSON.stringify(state.settings, (key, value: unknown) => (EXCLUDED as readonly string[]).includes(key) ? undefined : value),
    [state.settings],
  )

  useEffect(() => {
    if (!state.settingsHydrated || state.preferences) return
    if (skipSettingsSave.current) {
      skipSettingsSave.current = false
      return
    }
    invoke('set_settings', { settings: state.settings })
      .catch(fail)
  }, [saveKey, state.settingsHydrated, state.preferences, fail])

  useTauriEvent('get-info', () => openInfo())
  useTauriEvent('library-changed', () => dispatch({ type: 'refresh' }))
  useTauriEvent<string>('operation-error', (error) => dispatch({ type: 'error', error }))
  useTauriEvent('operation-recovered', () => dispatch({ type: 'clear-error' }))
  useTauriEvent<ConnectionState>('connection-changed', (connection) => dispatch({ type: 'connection', connection }))
  useTauriEvent<Settings>('settings-changed', (settings) => dispatch({ type: 'hydrateSettings', settings }))
  useTauriEvent<string>('sync-progress', (phase) => dispatch({ type: 'syncPhase', phase: phase || undefined }))
  useTauriEvent<{ tracks: number; fraction: number }>('sync-progress-count', (progress) => dispatch({ type: 'syncProgress', progress }))
  useTauriEvent('playlists-changed', () => dispatch({ type: 'playlistsRefresh' }))
  useTauriEvent('local-import-started', () => dispatch({ type: 'importStarted' }))
  useTauriEvent<ImportSummary>('local-import-complete', (summary) => dispatch({ type: 'importComplete', summary }))
  useTauriEvent('local-import-failed', () => dispatch({ type: 'importFailed' }))

  useEffect(() => {
    const unlisten = getCurrentWindow().onDragDropEvent(({ payload }) => {
      setNativeDragActive((active) => nextNativeDragActive(active, payload))
      if (payload.type === 'drop' && payload.paths.length) invoke('import_local', { paths: payload.paths })
        .catch(fail)
    })
    return () => { void unlisten.then((stop) => stop()) }
  }, [fail])

  useEffect(() => {
    if (!state.importStatus || state.importStatus === 'Importing local files…') return
    const timeout = window.setTimeout(() => dispatch({ type: 'clearImportStatus' }), 10_000)
    return () => window.clearTimeout(timeout)
  }, [state.importStatus])

  useEffect(() => {
    const query = state.query.trim()
    if (state.scope !== 'spotify' || !query || !state.connection.connected) {
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
          fail(error)
        })
    }, 300)
    return () => {
      active = false
      window.clearTimeout(timer)
    }
  }, [state.scope, state.query, state.connection.connected, fail])

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
    getCurrentWindow().setTitle(title).catch(fail)
  }, [state.source, fail])

  useEffect(() => {
    if (!state.playing?.simulated) return
    if (!state.playing?.isPlaying) return
    const currentIndex = playbackTracks.findIndex((track) => track.id === state.playing?.trackId)
    const current = playbackTracks[currentIndex]
    if (!current) return
    const next = playbackTracks[(currentIndex + 1) % playbackTracks.length]
    const timer = window.setInterval(() => {
      dispatch({ type: 'tick', duration: current.durationSecs, nextId: next.id })
    }, 1000)
    return () => window.clearInterval(timer)
  }, [state.connection.connected, state.playing?.trackId, state.playing?.isPlaying, playbackTracks])

  const mutate = (command: string, args: Record<string, unknown>) => {
    invoke(command, args)
      .then(() => dispatch({ type: 'refresh' }))
      .catch(fail)
  }
  const navigateSpotify = (track: Track, destination: 'album' | 'artist') => invoke<SpotifyNavEntry>('resolve_spotify_track_destination', { uri: track.uri, destination })
    .then((entry) => dispatch({ type: 'spotifyNavigate', entry }))
    .catch(fail)
  const setZoom = (zoom: number) => dispatch({
    type: 'settings',
    settings: { zoom: normalizeZoom(zoom, ZOOM_MIN, ZOOM_MAX) },
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
  const playingTrack = playbackTracks.find((track) => track.id === state.playing?.trackId)
  const selectedAlbum = state.sel.alb?.length === 1 ? state.sel.alb[0] : undefined

  useTauriEvent<string>('view-action', (payload) => {
    if (payload === 'zoom_in') setZoom(state.settings.zoom + 0.1)
    else if (payload === 'zoom_out') setZoom(state.settings.zoom - 0.1)
    else if (payload === 'actual_size') setZoom(1)
    else if (payload === 'toggle_zebra') dispatch({ type: 'settings', settings: { zebra: !state.settings.zebra } })
    else if (payload === 'toggle_browser') toggleBrowser()
    else if (payload.startsWith('theme_')) dispatch({ type: 'settings', settings: { theme: payload.slice(6) as Theme } })
  })
  useTauriEvent<string>('player-action', (payload) => {
    if (payload === 'play_pause') player.toggle()
    else player.step(payload === 'previous' ? -1 : 1)
  })
  useTauriEvent('open-preferences', openPreferences)
  useTauriEvent('open-setup', () => dispatch({ type: 'setup', open: true }))

  const onKeyDown = useRef<(event: KeyboardEvent) => void>(() => {})
  onKeyDown.current = (event: KeyboardEvent) => {
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
      const anchor = displayedTracks.some((track) => track.id === state.selectionAnchor)
        ? state.selectionAnchor
        : displayedTracks[0]?.id
      dispatch({ type: 'selection', ids: new Set(displayedTracks.map((track) => track.id)), anchor })
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
      typeahead.current.timer = window.setTimeout(() => { typeahead.current.buffer = '' }, 1000)
      const prefix = typeahead.current.buffer.toLocaleLowerCase()
      if (activePane === 'track') {
        const track = displayedTracks.find((track) => track.name.toLocaleLowerCase().startsWith(prefix))
        if (!track) return
        dispatch({ type: 'selectTrack', id: track.id })
        window.requestAnimationFrame(() => document.querySelector<HTMLElement>(`[data-track-id="${track.id}"]`)?.scrollIntoView({ block: 'nearest' }))
      } else {
        const facetValues = state.view?.facets[activePane === 'cat' ? 'cats' : activePane === 'art' ? 'arts' : 'albs'] ?? []
        const facetTitle = labels[state.source].facets[activePane === 'cat' ? 0 : activePane === 'art' ? 1 : 2]
        const index = facetValues.findIndex((value) => facetLabel(facetTitle, value).toLocaleLowerCase().startsWith(prefix))
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
      if (event.target instanceof Element && event.target.closest('.sidebar')) return
      event.preventDefault()
      const direction = event.key === 'ArrowUp' ? -1 : 1
      if (activePane === 'track') {
        if (!displayedTracks.length) return
        const current = displayedTracks.findIndex((track) => track.id === state.selectionAnchor)
        const index = current < 0 ? 0 : Math.max(0, Math.min(displayedTracks.length - 1, current + direction))
        dispatch({ type: 'selectTrack', id: displayedTracks[index].id })
        window.requestAnimationFrame(() => document.querySelector<HTMLElement>(`[data-track-id="${displayedTracks[index].id}"]`)?.scrollIntoView({ block: 'nearest' }))
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
  useEffect(() => {
    const handler = (event: KeyboardEvent) => onKeyDown.current(event)
    window.addEventListener('keydown', handler)
    return () => window.removeEventListener('keydown', handler)
  }, [])

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
  const playlistHiddenColumns = selectedPlaylist
    ? state.settings.playlistHiddenColumns[selectedPlaylist.id] ?? defaultSettings.hiddenColumns
    : defaultSettings.hiddenColumns

  return (
    <main className={`app-shell ${state.settings.zebra ? 'zebra' : ''}`} style={{ zoom: state.settings.zoom }}>
      <TransportBar
        playing={state.playing}
        track={playingTrack}
        query={state.query}
        scope={state.scope}
        volume={state.settings.volume}
        searchRef={search}
        onQuery={(query) => dispatch({ type: 'query', query })}
        onScope={(scope) => dispatch({ type: 'scope', scope })}
        onPlay={player.toggle}
        onPrev={() => player.step(-1)}
        onNext={() => player.step(1)}
        onVolume={(volume) => { dispatch({ type: 'settings', settings: { volume } }); player.setVolume(volume) }}
        onSeek={player.seek}
      />
      <div className="body-grid">
        <Sidebar
          state={state}
          playlists={playlists}
          onSource={(source) => { facetAnchors.current = {}; dispatch({ type: 'source', source }) }}
          onPlaylist={(id) => dispatch({ type: 'playlist', id })}
          onReorder={setPlaylists}
          onCollapse={() => dispatch({ type: 'settings', settings: { plCollapsed: !state.settings.plCollapsed } })}
          onShuffle={(shuffle) => invoke('set_shuffle', { shuffle }).then(() => dispatch({ type: 'settings', settings: { shuffle } })).catch(fail)}
          onRepeat={(repeat) => invoke('set_repeat', { mode: repeat }).then(() => dispatch({ type: 'settings', settings: { repeat } })).catch(fail)}
          onDrop={(id, subject) => addToPlaylist(id, subject).catch(fail)}
          onError={(error) => dispatch({ type: 'error', error })}
        />
        <section className="content">
          {state.connection.needs_reauth && <div className="startup-notice reauth-notice"><span>Spotify needs to be reconnected to enable playlists.</span><button onClick={() => invoke('connect_spotify').catch(fail)}>Reconnect</button></div>}
          {spotifySearchActive ? (
            state.connection.connected ? <SpotifySearch
              query={state.query.trim()}
              searching={state.spotifySearching}
              results={state.spotifyResults}
              navigation={state.spotifyNavigation}
              playingUri={state.playing?.uri ?? null}
              onAdd={(album) => invoke('add_spotify_album', album)
                .catch((error) => { fail(error); throw error })}
              onPlay={player.start}
              onPlaylist={setPlaylistSubject}
              onClose={() => dispatch({ type: 'scope', scope: 'library' })}
              onError={(error) => dispatch({ type: 'error', error })}
            /> : <div className="spotify-stub"><span>Connect to Spotify to search artists and albums.</span><button onClick={() => invoke('connect_spotify').catch(fail)}>Connect to Spotify</button></div>
          ) : selectedPlaylist ? <PlaylistView
            playlist={selectedPlaylist}
            revision={state.playlistRevision}
            libraryRevision={state.revision}
            playing={state.playing}
            columnOrder={state.settings.columnOrder}
            columnWidths={state.settings.columnWidths}
            hiddenColumns={playlistHiddenColumns}
            onPlay={player.start}
            onRate={(id, stars) => mutate('click_track_star', { id, stars })}
            onOpen={(target) => invoke('open_spotify_playlist', { id: selectedPlaylist.id, target }).catch(fail)}
            onPlaylist={setPlaylistSubject}
            onInfo={(tracks) => {
              if (tracks.length > 1 || tracks[0]?.id === null) dispatch({ type: 'info', info: { kind: 'multiple', tracks } })
              else openInfo(tracks[0]?.id)
            }}
            onHiddenColumns={(hiddenColumns) => dispatch({
              type: 'settings',
              settings: {
                playlistHiddenColumns: {
                  ...state.settings.playlistHiddenColumns,
                  [selectedPlaylist.id]: hiddenColumns,
                },
              },
            })}
            onError={(error) => dispatch({ type: 'error', error })}
          />
          : (
            <>
              <BrowserPane state={state} anchors={facetAnchors} onActivate={setActivePane} onSelect={selectFacet} onToggle={toggleBrowserPane} />
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
                tracks={displayedTracks}
                label={labels[state.source]}
                selectedIds={state.selectedTrackIds}
                playing={state.playing}
                columnOrder={state.settings.columnOrder}
                columnWidths={state.settings.columnWidths}
                hiddenColumns={state.settings.hiddenColumns}
                sortColumn={state.settings.sortColumn}
                sortDesc={state.settings.sortDesc}
                empty={libraryEmpty}
                onActivate={() => setActivePane('track')}
                onSetup={() => dispatch({ type: 'setup', open: true })}
                onClearSelection={() => dispatch({ type: 'selection', ids: new Set() })}
                onSelect={(id, event) => {
                  if (event.shiftKey && state.selectionAnchor !== undefined) {
                    const anchor = displayedTracks.findIndex((track) => track.id === state.selectionAnchor)
                    const row = displayedTracks.findIndex((track) => track.id === id)
                    if (anchor >= 0 && row >= 0) {
                      dispatch({
                        type: 'selection',
                        ids: new Set(displayedTracks.slice(Math.min(anchor, row), Math.max(anchor, row) + 1).map((track) => track.id)),
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
                onPlay={(id) => player.start(id, displayedTracks)}
                onEnabled={(id, enabled) => mutate('set_track_enabled', { id, enabled })}
                onRate={(id, stars) => mutate('click_track_star', { id, stars })}
                onInfo={openInfo}
                onPlaylist={setPlaylistSubject}
                onGoToAlbum={(track) => navigateSpotify(track, 'album')}
                onGoToArtist={(track) => navigateSpotify(track, 'artist')}
                onReorder={(columnOrder) => dispatch({ type: 'settings', settings: { columnOrder } })}
                onColumnWidths={(columnWidths) => dispatch({ type: 'settings', settings: { columnWidths } })}
                onHiddenColumns={(hiddenColumns) => dispatch({ type: 'settings', settings: { hiddenColumns } })}
                onSort={(sortColumn, sortDesc) => dispatch({ type: 'settings', settings: { sortColumn, sortDesc } })}
              />
            </>
          )}
          {state.error && <div className="error-banner">{state.error}</div>}
          <StatusBar view={view} unit={labels[state.source].item} syncPhase={state.syncPhase} syncProgress={state.syncProgress} importStatus={state.importStatus} empty={libraryEmpty} />
        </section>
      </div>
      {state.info?.kind === 'single' && <GetInfo key={state.info.track.id} track={state.info.track} onCancel={() => dispatch({ type: 'info' })} onSaved={() => {
        dispatch({ type: 'info' })
        dispatch({ type: 'refresh' })
      }} onError={(error) => dispatch({ type: 'error', error })} />}
      {state.info?.kind === 'multiple' && <MultipleItemInformation tracks={state.info.tracks} onCancel={() => dispatch({ type: 'info' })} onSaved={() => dispatch({ type: 'info' })} onError={(error) => dispatch({ type: 'error', error })} />}
      {state.setup && <SetupLibrary settings={state.settings} connected={state.connection.connected} onCancel={() => dispatch({ type: 'setup', open: false })} onConnect={(clientId) => saveSetupClientId(clientId)
        .then(() => invoke('connect_spotify'))
        .catch(fail)} onSync={(clientId) => saveSetupClientId(clientId)
        .then(() => {
          dispatch({ type: 'setup', open: false })
          return invoke('sync_from_spotify')
        })
        .catch(fail)} />}
      {state.preferences && <Preferences settings={state.settings} onZoom={setZoom} onCancel={cancelPreferences} onSave={({ browserPanes, ...settings }) => {
        const audioChanged = settings.streamingBitrate !== state.settings.streamingBitrate
          || settings.normalizeVolume !== state.settings.normalizeVolume
          || settings.gapless !== state.settings.gapless
        dispatch({ type: 'settings', settings })
        dispatch({ type: 'browserPanes', browserPanes })
        if (audioChanged) invoke('set_audio_settings', { streamingBitrate: settings.streamingBitrate, normalizeVolume: settings.normalizeVolume, gapless: settings.gapless })
          .catch(fail)
        dispatch({ type: 'preferences', open: false })
      }} />}
      {playlistSubject && <AddToPlaylist subject={playlistSubject} revision={state.playlistRevision} onAdd={addToPlaylist} onClose={() => setPlaylistSubject(undefined)} onError={(error) => dispatch({ type: 'error', error })} />}
      {nativeDragActive && <div className="native-drop-overlay"><strong>Drop to add to Library</strong><span>Audio files and folders</span></div>}
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

function TransportBar({ playing, track, query, scope, volume, searchRef, onQuery, onScope, onPlay, onPrev, onNext, onVolume, onSeek }: {
  playing: State['playing']; track?: PlaybackTrack; query: string; scope: State['scope']
  volume: number
  searchRef: React.RefObject<HTMLInputElement | null>
  onQuery: (query: string) => void; onScope: (scope: State['scope']) => void; onSeek: (seconds: number) => void
  onPlay: () => void; onPrev: () => void; onNext: () => void; onVolume: (volume: number) => void
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
  return <header className="transport">
    <div className="transport-controls">
      <div className="transport-buttons">
        <button aria-label="Previous track" onClick={onPrev}>⏮</button>
        <button className="play-button" aria-label={playing?.isPlaying ? 'Pause' : 'Play'} onClick={onPlay}>{playing?.isPlaying ? '⏸' : '▶'}</button>
        <button aria-label="Next track" onClick={onNext}>⏭</button>
      </div>
      <label className="volume-control"><span aria-hidden="true">−</span><input aria-label="Volume" type="range" min="0" max="100" value={volume} style={{ '--volume': `${volume}%` } as React.CSSProperties} onChange={(event) => onVolume(Number(event.target.value))} /><span aria-hidden="true">+</span></label>
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
      <input ref={searchRef} className="search" type="search" value={query} onChange={(event) => onQuery(event.target.value)} placeholder={`⌕ Search ${scope === 'library' ? 'Library' : 'Spotify'}`} />
      <div className="search-scope">
        <div className="scope-pills" aria-label="Search scope">
          <button className={scope === 'library' ? 'active' : ''} onClick={() => onScope('library')}>Library</button>
          <button className={scope === 'spotify' ? 'active' : ''} onClick={() => onScope('spotify')}>Spotify</button>
        </div>
      </div>
    </div>
  </header>
}

function Sidebar({ state, playlists, onSource, onPlaylist, onReorder, onCollapse, onShuffle, onRepeat, onDrop, onError }: {
  state: State
  playlists?: PlaylistListView[]
  onSource: (source: Source) => void
  onPlaylist: (id: string) => void
  onReorder: (playlists: PlaylistListView[]) => void
  onCollapse: () => void
  onShuffle: (shuffle: boolean) => void
  onRepeat: (repeat: RepeatMode) => void
  onDrop: (id: string, subject: PlaylistSubject) => void
  onError: (error: string) => void
}) {
  const [creating, setCreating] = useState(false)
  const [name, setName] = useState('')
  const [dropTarget, setDropTarget] = useState<string>()
  const [dragging, setDragging] = useState<string>()
  const [insertBefore, setInsertBefore] = useState<number>()
  const [menu, setMenu] = useState<{ x: number; y: number; playlist: PlaylistListView }>()
  const [confirming, setConfirming] = useState<PlaylistListView>()
  const [busy, setBusy] = useState(false)
  const playlistDrag = useRef<{ id: string; pointerId: number; startY: number; moved: boolean } | undefined>(undefined)
  const dragInsertBefore = useRef<number | undefined>(undefined)
  const suppressPlaylistClick = useRef(false)
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
  const reorder = async (dragged: string, target: number) => {
    if (!playlists) return
    const ids = moveToIndex(playlists.map((playlist) => playlist.id), dragged, target)
    const reordered = ids.map((id) => playlists.find((playlist) => playlist.id === id)!)
    setInsertBefore(undefined)
    onReorder(reordered)
    try { await invoke('reorder_playlists', { ids }) }
    catch (error) { onReorder(playlists); onError(String(error)) }
  }
  const cancelPlaylistDrag = () => {
    playlistDrag.current = undefined
    dragInsertBefore.current = undefined
    setDragging(undefined)
    setInsertBefore(undefined)
  }
  const unfollow = async () => {
    if (!confirming) return
    setBusy(true)
    try {
      await invoke('playlist_unfollow', { id: confirming.id })
      setConfirming(undefined)
    } catch (error) {
      onError(String(error))
    } finally {
      setBusy(false)
    }
  }
  return <><aside className="sidebar" tabIndex={0} onKeyDown={(event) => {
    if (event.target instanceof HTMLInputElement || (event.key !== 'ArrowUp' && event.key !== 'ArrowDown')) return
    event.preventDefault()
    const rows = [...event.currentTarget.querySelectorAll<HTMLButtonElement>('.source-row, .playlist-row')]
    const current = rows.findIndex((row) => row.classList.contains('active'))
    const next = rows[current + (event.key === 'ArrowUp' ? -1 : 1)]
    next?.click()
    next?.scrollIntoView({ block: 'nearest' })
  }}>
    <div className="section-label">Library</div>
    {(Object.keys(labels) as Source[]).map((source) => <button key={source} className={`source-row ${state.source === source && !state.selectedPlaylist ? 'active' : ''}`} onClick={() => onSource(source)}>
      <span>{labels[source].icons}</span><span>{labels[source].name}</span><span className="source-count">{state.view?.counts.perSource[source] ?? '—'}</span>
    </button>)}
    <button className="section-label playlists-label" aria-expanded={!state.settings.plCollapsed} onClick={onCollapse}><span className={`disclosure ${state.settings.plCollapsed ? 'collapsed' : ''}`} aria-hidden="true">▾</span><span>Playlists</span></button>
    <div className="playlist-list">
    {!state.settings.plCollapsed && creating && <div className="playlist-new-row"><input autoFocus aria-label="Playlist name" value={name} onChange={(event) => setName(event.target.value)} onKeyDown={(event) => {
      if (event.key === 'Enter') void create()
      else if (event.key === 'Escape') { setCreating(false); setName('') }
    }} /></div>}
    {!state.settings.plCollapsed && playlists?.map((playlist, index) => <Fragment key={playlist.id}><button
      className={`playlist-row ${state.selectedPlaylist === playlist.id || dropTarget === playlist.id ? 'active' : ''} ${dragging === playlist.id ? 'dragging' : ''} ${insertBefore === index ? 'insert-before' : ''} ${insertBefore === playlists.length && index === playlists.length - 1 ? 'insert-after' : ''}`}
      onClick={(event) => {
        if (suppressPlaylistClick.current) { suppressPlaylistClick.current = false; event.preventDefault(); return }
        onPlaylist(playlist.id)
      }}
      onPointerDown={(event) => {
        if (event.button !== 0) return
        suppressPlaylistClick.current = false
        playlistDrag.current = { id: playlist.id, pointerId: event.pointerId, startY: event.clientY, moved: false }
        event.currentTarget.setPointerCapture(event.pointerId)
      }}
      onPointerMove={(event) => {
        const drag = playlistDrag.current
        if (!drag || drag.pointerId !== event.pointerId || (!drag.moved && Math.abs(event.clientY - drag.startY) < 4)) return
        drag.moved = true
        event.preventDefault()
        setDragging(drag.id)
        const rows = [...event.currentTarget.parentElement!.querySelectorAll<HTMLElement>('.playlist-row')]
        const target = insertionIndexAtY(rows.map((row) => { const bounds = row.getBoundingClientRect(); return bounds.top + bounds.height / 2 }), event.clientY)
        dragInsertBefore.current = target
        setInsertBefore(target)
      }}
      onPointerUp={(event) => {
        const drag = playlistDrag.current
        if (!drag || drag.pointerId !== event.pointerId) return
        const target = dragInsertBefore.current
        const moved = drag.moved
        cancelPlaylistDrag()
        if (!moved || target === undefined) return
        event.preventDefault()
        suppressPlaylistClick.current = true
        window.setTimeout(() => { suppressPlaylistClick.current = false }, 0)
        void reorder(drag.id, target)
      }}
      onPointerCancel={cancelPlaylistDrag}
      onDragOver={(event) => {
        if (!playlist.owned) return
        if (!event.dataTransfer.types.includes(DRAG_TYPE)) return
        if (event.dataTransfer.types.includes(DRAG_LOCAL_TYPE)) { setDropTarget(undefined); return }
        event.preventDefault()
        event.dataTransfer.dropEffect = 'copy'
        setDropTarget(playlist.id)
      }}
      onDragLeave={(event) => { if (!event.currentTarget.contains(event.relatedTarget as Node | null)) setDropTarget(undefined) }}
      onDrop={(event) => {
        event.preventDefault()
        if (!playlist.owned) return
        setDropTarget(undefined)
        try {
          const subject = JSON.parse(event.dataTransfer.getData(DRAG_TYPE)) as PlaylistSubject
          if (hasLocalTracks(subject)) onError(LOCAL_PLAYLIST_HINT)
          else onDrop(playlist.id, subject)
        }
        catch { onError('Could not read the dragged playlist item.') }
      }}
      onContextMenu={(event) => { event.preventDefault(); setMenu({ x: event.clientX, y: event.clientY, playlist }) }}
    >
      <span>{playlist.owned ? '' : '🌐'}</span><span title={playlist.name}>{playlist.name}</span><span className="source-count">{playlist.trackCount}</span>
    </button>{menu?.playlist.id === playlist.id && <ContextMenu x={menu.x} y={menu.y} onClose={() => setMenu(undefined)}><button onClick={() => { setConfirming(playlist); setMenu(undefined) }}>{playlist.owned ? 'Delete Playlist…' : 'Unfollow Playlist…'}</button></ContextMenu>}</Fragment>)}
    </div>
    <div className="sidebar-actions">
      <button data-tooltip="New playlist" aria-label="New playlist" onClick={() => { setCreating(true); if (state.settings.plCollapsed) onCollapse() }}><svg aria-hidden="true" width="14" height="14" viewBox="0 0 18 18" fill="none" stroke="currentColor" strokeWidth="1.9" strokeLinecap="round"><path d="M9 3.6v10.8M3.6 9h10.8" /></svg></button>
      <button className={state.settings.shuffle ? 'active' : ''} data-tooltip={`Shuffle: ${state.settings.shuffle ? 'on' : 'off'}`} aria-label={`Shuffle: ${state.settings.shuffle ? 'on' : 'off'}`} aria-pressed={state.settings.shuffle} onClick={() => onShuffle(!state.settings.shuffle)}><svg aria-hidden="true" width="15" height="15" viewBox="0 0 18 18" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round"><path d="M2.6 5h3.2C7 5 7.9 6.4 9 9s2 4 3.2 4h2.6M2.6 13h3.2C7 13 7.9 11.6 9 9s2-4 3.2-4h2.6M12.9 3.1 15.1 5l-2.2 1.9M12.9 11.1l2.2 1.9-2.2 1.9" /></svg></button>
      <button className={state.settings.repeat !== 'off' ? 'active' : ''} data-tooltip={`Repeat: ${state.settings.repeat}`} aria-label={`Repeat: ${state.settings.repeat}`} aria-pressed={state.settings.repeat !== 'off'} onClick={() => onRepeat(state.settings.repeat === 'off' ? 'all' : state.settings.repeat === 'all' ? 'one' : 'off')}><svg aria-hidden="true" width="15" height="15" viewBox="0 0 18 18" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round"><path d="M3.9 9.4V7.2a3 3 0 0 1 3-3h6.5M11.6 2.4l2.1 1.8L11.6 6M14.1 8.6v2.2a3 3 0 0 1-3 3H4.6M6.4 12l-2.1 1.8 2.1 1.8" /></svg>{state.settings.repeat === 'one' && <span className="repeat-one-badge" aria-hidden="true">1</span>}</button>
    </div>
  </aside>{confirming && <ModalDialog className="get-info playlist-confirm" labelledBy="playlist-confirm-title" onCancel={busy ? undefined : () => setConfirming(undefined)} onSubmit={busy ? undefined : unfollow} closeOnBackdrop><h2 id="playlist-confirm-title">{confirming.owned ? 'Delete Playlist?' : 'Unfollow Playlist?'}</h2><p>{confirming.owned ? `Delete “${confirming.name}” from Spotify?` : `Stop following “${confirming.name}”?`}</p><div className="modal-actions"><button type="button" autoFocus disabled={busy} onClick={() => setConfirming(undefined)}>Cancel</button><button type="submit" className="danger" disabled={busy}>{busy ? 'Working…' : confirming.owned ? 'Delete' : 'Unfollow'}</button></div></ModalDialog>}</>
}

function PlaylistView({ playlist, revision, libraryRevision, playing, columnOrder, columnWidths, hiddenColumns, onPlay, onRate, onOpen, onPlaylist, onInfo, onHiddenColumns, onError }: {
  playlist: PlaylistListView
  revision: number
  libraryRevision: number
  playing: State['playing']
  columnOrder: ColumnKey[]
  columnWidths: Partial<Record<ColumnKey, number>>
  hiddenColumns: ColumnKey[]
  onPlay: (id: number, tracks: readonly PlaybackTrack[]) => void
  onRate: (id: number, stars: number) => void
  onOpen: (target: 'app' | 'web') => void
  onPlaylist: (subject: PlaylistSubject) => void
  onInfo: (tracks: PlaylistTrack[]) => void
  onHiddenColumns: (columns: ColumnKey[]) => void
  onError: (error: string) => void
}) {
  const [tracks, setTracks] = useState<PlaylistTrack[]>([])
  const [selected, setSelected] = useState<Set<number>>(new Set())
  const [selectionAnchor, setSelectionAnchor] = useState<number>()
  const [insertBefore, setInsertBefore] = useState<number>()
  const [mutating, setMutating] = useState(false)
  const [sortColumn, setSortColumn] = useState<ColumnKey | null>(null)
  const [sortDesc, setSortDesc] = useState(false)
  const [menu, setMenu] = useState<{ x: number; y: number; upstreamIndex?: number }>()
  const onErrorRef = useRef(onError)
  onErrorRef.current = onError
  const canChangePlaylist = playlist.owned && tracks.length === playlist.trackCount
  const canReorder = canChangePlaylist && sortColumn === null
  useEffect(() => {
    if (!playlist.itemsAvailable) {
      setTracks([])
      return
    }
    let active = true
    setSelected(new Set())
    setSelectionAnchor(undefined)
    invoke<PlaylistTrack[]>('playlist_tracks', { id: playlist.id })
      .then((rows) => active && setTracks(rows))
      .catch((error) => active && onErrorRef.current(String(error)))
    return () => { active = false }
  }, [playlist.id, playlist.itemsAvailable, revision, libraryRevision])
  useEffect(() => {
    setSortColumn(null)
    setSortDesc(false)
  }, [playlist.id])
  useEffect(() => {
    const selectAll = (event: KeyboardEvent) => {
      if (!(event.metaKey || event.ctrlKey) || event.key.toLowerCase() !== 'a' || (event.target as Element | null)?.closest('input, textarea, select')) return
      event.preventDefault()
      setSelected(new Set(tracks.map((_, index) => index)))
      setSelectionAnchor(tracks.length ? 0 : undefined)
    }
    window.addEventListener('keydown', selectAll)
    return () => window.removeEventListener('keydown', selectAll)
  }, [tracks])
  const rows = playlistRows(tracks, sortColumn, sortDesc)
  const queue: PlaybackTrack[] = rows.map(({ track, upstreamIndex }) => ({ ...track, id: track.id ?? SYNTHETIC_BASE + upstreamIndex }))
  const headings = { ...trackColumnHeadings(labels.music), track: 'Track' }
  const visibleColumns = columnOrder.filter((column) => !hiddenColumns.includes(column))
  const columns = trackGridColumns(visibleColumns, columnWidths, '35px')
  const select = (upstreamIndex: number, event: React.MouseEvent) => {
    if (event.shiftKey && selectionAnchor !== undefined) {
      const anchor = rows.findIndex((row) => row.upstreamIndex === selectionAnchor)
      const current = rows.findIndex((row) => row.upstreamIndex === upstreamIndex)
      if (anchor >= 0 && current >= 0) {
        setSelected(new Set(rows.slice(Math.min(anchor, current), Math.max(anchor, current) + 1).map((row) => row.upstreamIndex)))
        return
      }
    }
    if (event.metaKey || event.ctrlKey) {
      const next = new Set(selected)
      if (!next.delete(upstreamIndex)) next.add(upstreamIndex)
      setSelected(next)
      setSelectionAnchor(upstreamIndex)
    } else {
      setSelected(new Set([upstreamIndex]))
      setSelectionAnchor(upstreamIndex)
    }
  }
  const selectedIndices = (target?: number) => {
    const indices = target !== undefined && !selected.has(target) ? [target] : [...selected]
    return indices.sort((left, right) => left - right)
  }
  const drop = async (event: React.DragEvent, index: number) => {
    const range = parseDragRange(event.dataTransfer.getData(PLAYLIST_TRACK_DRAG_TYPE))
    if (!range) return
    if (index >= range.start && index <= range.start + range.length) {
      setInsertBefore(undefined)
      return
    }
    setMutating(true)
    setInsertBefore(undefined)
    try {
      await invoke('playlist_reorder', { id: playlist.id, rangeStart: range.start, insertBefore: index, rangeLength: range.length })
    } catch (error) {
      onError(String(error))
    } finally {
      setSelected(new Set())
      setSelectionAnchor(undefined)
      setMutating(false)
    }
  }
  const remove = async (target?: number) => {
    const indices = selectedIndices(target)
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
  const addToLibrary = async (target?: number) => {
    const uris = selectedIndices(target).map((index) => tracks[index]).filter((track) => track.id === null).map((track) => track.uri)
    if (!uris.length) return
    setMenu(undefined)
    setMutating(true)
    try {
      await invoke('add_spotify_tracks', { uris })
    } catch (error) {
      onError(String(error))
    } finally {
      setMutating(false)
    }
  }
  const addToAnotherPlaylist = (target?: number) => {
    const chosen = selectedIndices(target).map((index) => tracks[index])
    if (!chosen.length) return
    setMenu(undefined)
    onPlaylist({ kind: 'tracks', label: chosen.length === 1 ? `Track · ${chosen[0].name}` : `${chosen.length} tracks`, uris: chosen.map((track) => track.uri) })
  }
  const getInfo = (target?: number) => {
    const chosen = selectedIndices(target).map((index) => tracks[index])
    if (!chosen.length) return
    setMenu(undefined)
    onInfo(chosen)
  }
  return <div className="playlist-view">
    <header className="playlist-header"><strong>{playlist.name}</strong><span>{playlist.trackCount} {playlist.trackCount === 1 ? 'track' : 'tracks'}{playlist.owner ? ` · by ${playlist.owner}` : ''}{sortColumn ? ` · sorted by ${headings[sortColumn]}` : ''}</span>{playlist.owned && <button disabled={!canChangePlaylist || !selected.size || mutating} onClick={() => void remove()}>Remove</button>}</header>
    {!playlist.itemsAvailable ? <div className="playlist-unavailable"><strong>Tracks unavailable in Retune</strong><span>Spotify does not allow third-party apps to interact with playlists not owned by you. :-(</span><div className="playlist-open-actions"><button onClick={() => onOpen('app')}>Open in Spotify app</button><button onClick={() => onOpen('web')}>Open on Spotify Web</button></div></div> : <>
    <div className="playlist-track-scroll" onClick={(event) => {
      if (event.target !== event.currentTarget && !(event.target as Element).closest('.playlist-end-drop')) return
      setSelected(new Set())
      setSelectionAnchor(undefined)
    }}>
      <div className="playlist-track-header track-header" style={{ gridTemplateColumns: columns }} onContextMenu={(event) => { event.preventDefault(); setMenu({ x: event.clientX, y: event.clientY }) }}>
        <button aria-label="Restore Spotify playlist order" title="Spotify order · click to restore" className={sortColumn === null ? 'active' : ''} onClick={() => { setSortColumn(null); setSortDesc(false) }}>#</button>
        {visibleColumns.map((column) => <button key={column} className={COLUMN_SPECS[column].numeric ? 'track-number' : ''} onClick={() => {
          setSortDesc(sortColumn === column ? !sortDesc : false)
          setSortColumn(column)
        }}>{headings[column]}{sortColumn === column ? sortDesc ? ' ▼' : ' ▲' : ''}</button>)}
      </div>
      {rows.map(({ track, upstreamIndex }, rowIndex) => <div
        key={`${track.uri}-${upstreamIndex}`}
        className={`playlist-track-row track-row ${selected.has(upstreamIndex) ? 'selected' : ''} ${insertBefore === upstreamIndex ? 'insert-before' : ''} ${isCurrentTrack(playing, queue[rowIndex]) ? 'playing' : ''}`}
        style={{ gridTemplateColumns: columns }}
        draggable={canReorder && !mutating}
        onClick={(event) => select(upstreamIndex, event)}
        onDoubleClick={() => onPlay(queue[rowIndex].id, queue)}
        onDragStart={canReorder ? (event) => {
          const dragged = selected.has(upstreamIndex) ? [...selected].sort((left, right) => left - right) : [upstreamIndex]
          if (dragged.some((row, offset) => row !== dragged[0] + offset)) {
            event.preventDefault()
            onError('Select a contiguous block of tracks to reorder.')
            return
          }
          event.dataTransfer.effectAllowed = 'move'
          event.dataTransfer.setData(PLAYLIST_TRACK_DRAG_TYPE, JSON.stringify({ start: dragged[0], length: dragged.length }))
        } : undefined}
        onDragOver={canReorder ? (event) => { event.preventDefault(); setInsertBefore(upstreamIndex) } : undefined}
        onDrop={canReorder ? (event) => { event.preventDefault(); void drop(event, upstreamIndex) } : undefined}
        onDragEnd={() => setInsertBefore(undefined)}
        onContextMenu={(event) => {
          event.preventDefault()
          if (!selected.has(upstreamIndex)) select(upstreamIndex, event)
          setMenu({ x: event.clientX, y: event.clientY, upstreamIndex })
        }}
      ><span className="track-number">{upstreamIndex + 1}</span>{visibleColumns.map((column) => <TrackCell key={column} track={track} column={column} playing={isCurrentTrack(playing, queue[rowIndex]) ? playing?.isPlaying ? 'playing' : 'paused' : false} selected={selected.has(upstreamIndex)} onRate={track.id === null ? undefined : onRate.bind(null, track.id)} />)}</div>)}
      {canReorder && <div className={`playlist-end-drop ${insertBefore === tracks.length ? 'insert-before' : ''}`} onDragOver={(event) => { event.preventDefault(); setInsertBefore(tracks.length) }} onDrop={(event) => { event.preventDefault(); void drop(event, tracks.length) }} />}
    </div>
    {menu && (menu.upstreamIndex === undefined
      ? <CheckboxMenu x={menu.x} y={menu.y} onClose={() => setMenu(undefined)} items={columnOrder.map((column) => ({ key: column, label: headings[column], checked: !hiddenColumns.includes(column), disabled: column === 'name', onChange: (checked) => onHiddenColumns(checked ? hiddenColumns.filter((hidden) => hidden !== column) : [...hiddenColumns, column]) }))} />
      : <ContextMenu x={menu.x} y={menu.y} onClose={() => setMenu(undefined)}>
        <button onClick={() => { setSelected(new Set(tracks.map((_, index) => index))); setSelectionAnchor(tracks.length ? 0 : undefined); setMenu(undefined) }}>Select All</button>
        <button disabled={mutating || !selectedIndices(menu.upstreamIndex).some((index) => tracks[index].id === null)} onClick={() => void addToLibrary(menu.upstreamIndex)}>Add to Library</button>
        <button onClick={() => addToAnotherPlaylist(menu.upstreamIndex)}>Add to Playlist…</button>
        <button onClick={() => getInfo(menu.upstreamIndex)}>Get Info</button>
        {playlist.owned && <button disabled={!canChangePlaylist || mutating} onClick={() => { setMenu(undefined); void remove(menu.upstreamIndex) }}>Remove from Playlist…</button>}
      </ContextMenu>)}
    </>}
  </div>
}

function AddToPlaylist({ subject, revision, onAdd, onClose, onError }: {
  subject: PlaylistSubject
  revision: number
  onAdd: (id: string, subject: PlaylistSubject) => Promise<unknown>
  onClose: () => void
  onError: (error: string) => void
}) {
  const local = hasLocalTracks(subject)
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
  return <ModalDialog className="playlist-popover" labelledBy="add-to-playlist-title" onCancel={onClose} closeOnBackdrop>
      <header><h2 id="add-to-playlist-title">Add to Playlist</h2><span>{subject.label}</span></header>
      {local && <p className="playlist-local-hint">{LOCAL_PLAYLIST_HINT}</p>}
      <div className="playlist-popover-list">{playlists.map((playlist) => <button type="button" key={playlist.id} disabled={local || !playlist.owned || busy === playlist.id} onClick={() => void add(playlist.id)}>
        <span>{playlist.contains ? '✓' : ''}</span><span>{playlist.owned ? '' : '🌐'}</span><strong>{playlist.name}</strong>{!playlist.owned && <small>{playlist.owner}</small>}
      </button>)}</div>
      <footer>{creating
        ? <input autoFocus aria-label="Playlist name" value={name} onChange={(event) => setName(event.target.value)} onKeyDown={(event) => {
          if (event.key === 'Enter') void create()
        }} />
        : <button type="button" className="new-playlist-button" disabled={local} onClick={() => setCreating(true)}>+ New Playlist</button>}
        <button type="button" className="done-button" onClick={onClose}>Done</button></footer>
  </ModalDialog>
}

function StatusBar({ view, unit, syncPhase, syncProgress, importStatus, empty }: { view: BrowseView | null; unit: string; syncPhase?: string; syncProgress?: { tracks: number; fraction: number }; importStatus?: string; empty: boolean }) {
  const total = view?.counts.totalSecs ?? 0
  const hours = Math.floor(total / 3600)
  const minutes = Math.floor((total % 3600) / 60)
  const count = view?.counts.tracks ?? 0
  return <footer className="status-bar">{syncProgress
    ? <span className="sync-status"><span>⟳ Syncing from Spotify…</span><progress className="sync-meter" max={1} value={syncProgress.fraction} /><span>{syncProgress.tracks} tracks synced</span></span>
    : <span>{syncPhase ?? importStatus ?? (empty ? 'No library — set up to begin' : `${count} ${count === 1 ? unit : `${unit}s`}, ${hours}:${String(minutes).padStart(2, '0')} hours`)}</span>}</footer>
}

export default App
