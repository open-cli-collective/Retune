import { listen } from '@tauri-apps/api/event'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { Fragment, useCallback, useEffect, useEffectEvent, useMemo, useReducer, useRef, useState } from 'react'
import './App.css'
import { appliedZoom, beginPendingEntity, beginRequestGeneration, browseRequestKey, browseViewForRequest, cancelTrackInfoLoad, COLUMN_SPECS, compareTracks, contiguousRange, currentPlaybackAuthorization, currentPlaylistRows, DRAG_LOCAL_TYPE, DRAG_TYPE, entityRequestGeneration, facetLabel, failedPlaylistRows, formatTime, hasLocalTracks, insertionIndexAtY, isCurrentRequestGeneration, isCurrentTrack, labels, loadArtwork, loadCurrentGeneration, loadingPlaylistRows, moveBefore, moveToIndex, normalizeZoom, pendingEntities, pendingPlaybackTarget, playbackOriginAction, playbackQueue, playbackRetryReady, playbackStartAction, playlistLayoutFor, playlistOverride, playlistRows, playlistRowsReady, PLAYLIST_COLUMNS, PLAYLIST_DEFAULT_COLUMN_ORDER, PLAYLIST_DEFAULT_HIDDEN_COLUMNS, resolvedPlaylistRows, resizedColumnWidth, routeGlobalShortcut, selectionAfterFacet, simulatedPlaybackTick, staleSelectionFacet, SYNTHETIC_BASE, trackColumnHeadings, trackGridColumns, visibleColumnOrder } from './ui.ts'
import { defaultSettings, initialState, reducer, type Action, type State } from './appState.ts'
import { GetInfo, MultipleItemInformation, PlaybackAuthorization, Preferences, SetupLibrary } from './dialogViews.tsx'
import { AlbumRatingStrip, BrowserPane, TrackCell, TrackList } from './libraryViews.tsx'
import { SpotifyPageBack, SpotifySearch } from './spotifyViews.tsx'
import type { ActivePane, BrowseView, BrowserPanes, ColumnKey, LastFmImportState, PlaybackOrigin, PlaybackTrack, Playing, PlaylistListView, PlaylistSubject, PlaylistTrack, RepeatMode, Selection, SettingsPatch, Source, Theme, Track } from './types.ts'
import { CheckboxMenu, ContextMenu, ModalDialog } from './viewShared.tsx'
import { importDownloadPercent, importDownloadProgressLabel, importStatusText } from './lastfmImportState.ts'
import { libraryEvents, libraryGateway } from './libraryGateway.ts'
import { playbackEvents, playbackGateway } from './playbackGateway.ts'
import { spotifyEvents, spotifyGateway } from './spotifyGateway.ts'
import { dispatchMainEvent, subscribeInvalidationThenSnapshot, subscribeMainEvents, type MainEventHandlers } from './ipc.ts'
import { appGateway } from './appGateway.ts'
import { lastfmGateway } from './lastfmGateway.ts'

const LOCAL_PLAYLIST_HINT = "Selection includes local files — Spotify playlists can't contain them."

const emptyTracks: Track[] = []
const ZOOM_MIN = 0.7
const ZOOM_MAX = 1.8
const ZOOM_BASE = 1.15
function useTauriEvent<T = unknown>(event: string, handler: (payload: T) => void) {
  const ref = useRef(handler)
  ref.current = handler
  useEffect(() => {
    const sub = listen<T>(event, ({ payload }) => ref.current(payload))
    return () => { void sub.then((stop) => stop()) }
  }, [event])
}

function useTauriInvalidationSnapshot<T>(event: string, snapshot: () => Promise<T>, handler: (payload: T) => void) {
  const handlerRef = useRef(handler)
  const snapshotRef = useRef(snapshot)
  handlerRef.current = handler
  snapshotRef.current = snapshot
  useEffect(() => {
    let active = true
    const subscription = subscribeInvalidationThenSnapshot(
      (invalidate) => listen(event, invalidate),
      () => snapshotRef.current(),
      (value) => handlerRef.current(value),
      () => active,
    )
    return () => {
      active = false
      void subscription.then((stop) => stop())
    }
  }, [event])
}

function usePlayer(connected: boolean, playbackAuthorized: boolean, playing: Playing | null, dispatch: React.Dispatch<Action>) {
  const queue = useRef<readonly PlaybackTrack[]>(emptyTracks)
  const origin = useRef<PlaybackOrigin | undefined>(undefined)
  const pendingPlay = useRef<{ id: number; tracks: readonly PlaybackTrack[]; origin?: PlaybackOrigin; awaitingPlaybackAuthorization: boolean } | null>(null)
  const playGeneration = useRef(0)
  const playingRef = useRef(playing)
  const volumeTimer = useRef<number>(undefined)
  playingRef.current = playing

  const onState = useCallback((player: import('./types.ts').PlayerState) => {
    if (player.external) origin.current = undefined
    dispatch({ type: 'playerState', player, queue: queue.current, origin: origin.current })
  }, [dispatch])

  const onAuthorizationRequired = useCallback((prompt: import('./types.ts').PlaybackAuthorizationPrompt) => {
    const id = pendingPlaybackTarget(prompt, queue.current)
    if (id === null) return
    beginRequestGeneration(playGeneration)
    pendingPlay.current = { id, tracks: queue.current, origin: origin.current, awaitingPlaybackAuthorization: true }
    dispatch({ type: 'playbackAuthorization', prompt })
  }, [dispatch])

  const run = useCallback((request: () => Promise<unknown>) => {
    request().catch((error) => dispatch({ type: 'error', error: String(error) }))
  }, [dispatch])

  const liveBackend = useCallback(() => {
    const current = playingRef.current
    return !current?.simulated && (connected || current?.uri?.startsWith('file:'))
  }, [connected])

  const start = useCallback((id: number, tracks: readonly PlaybackTrack[], launchOrigin?: PlaybackOrigin) => {
    const playable = playbackQueue(tracks, id)
    const target = playable.find((track) => track.id === id)
    const request = beginRequestGeneration(playGeneration)
    queue.current = playable
    origin.current = launchOrigin
    pendingPlay.current = null
    dispatch({ type: 'playbackAuthorization', prompt: null })
    if (playbackStartAction(target?.uri, connected) === 'connect') {
      // Kick off the OAuth flow instead of erroring; the pending play fires
      // once connection-changed reports connected.
      pendingPlay.current = { id, tracks: playable, origin: launchOrigin, awaitingPlaybackAuthorization: false }
      run(spotifyGateway.connect)
      return
    }
    if (target?.uri.startsWith('file:') || target?.uri.startsWith('spotify:')) {
      playbackGateway.play(playable, playable.findIndex((track) => track.id === id))
        .then((outcome) => {
          const authorization = currentPlaybackAuthorization(request, playGeneration, outcome, playable)
          if (!authorization) return
          pendingPlay.current = { id: authorization.id, tracks: playable, origin: launchOrigin, awaitingPlaybackAuthorization: true }
          dispatch({ type: 'playbackAuthorization', prompt: authorization.prompt })
        })
        .catch((error) => {
          if (isCurrentRequestGeneration(request, playGeneration)) dispatch({ type: 'error', error: String(error) })
        })
      return
    }
    dispatch({ type: 'play', id, queue: playable, origin: launchOrigin })
  }, [connected, dispatch, run])

  useEffect(() => {
    if (!connected || !pendingPlay.current) return
    if (!playbackRetryReady(connected, playbackAuthorized, pendingPlay.current.awaitingPlaybackAuthorization)) return
    const { id, tracks, origin: launchOrigin } = pendingPlay.current
    pendingPlay.current = null
    start(id, tracks, launchOrigin)
  }, [connected, playbackAuthorized, start])

  const cancelPending = useCallback(() => {
    beginRequestGeneration(playGeneration)
    pendingPlay.current = null
    dispatch({ type: 'playbackAuthorization', prompt: null })
  }, [dispatch])

  const toggle = useCallback(() => {
    if (liveBackend()) {
      if (playingRef.current && !playingRef.current.external) run(playbackGateway.toggle)
    }
    else dispatch({ type: 'togglePlay' })
  }, [dispatch, liveBackend, run])

  const step = useCallback((direction: number) => {
    const current = playingRef.current
    if (liveBackend()) {
      if (current && !current.external) run(direction < 0 ? playbackGateway.previous : playbackGateway.next)
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
    volumeTimer.current = window.setTimeout(() => run(() => playbackGateway.setVolume(volume)), 150)
  }, [liveBackend, run])

  const seek = useCallback((seconds: number) => {
    if (liveBackend()) {
      if (playingRef.current && !playingRef.current.external) run(() => playbackGateway.seek(seconds))
      return
    }
    dispatch({ type: 'seek', elapsed: seconds })
  }, [dispatch, liveBackend, run])

  useEffect(() => () => window.clearTimeout(volumeTimer.current), [])

  return useMemo(
    () => ({ start, toggle, step, setVolume, seek, cancelPending, onState, onAuthorizationRequired }),
    [cancelPending, onAuthorizationRequired, onState, seek, setVolume, start, step, toggle],
  )
}

function App() {
  const [state, dispatch] = useReducer(reducer, initialState)
  const fail = useCallback((error: unknown) => dispatch({ type: 'error', error: String(error) }), [])
  const [nativeDragActive, setNativeDragActive] = useState(false)
  const [activePane, setActivePane] = useState<ActivePane>('track')
  const [playlists, setPlaylists] = useState<PlaylistListView[]>()
  const [playlistSubject, setPlaylistSubject] = useState<PlaylistSubject>()
  const [artworkOpen, setArtworkOpen] = useState(false)
  const [browserPlayKey, setBrowserPlayKey] = useState<string>()
  const search = useRef<HTMLInputElement>(null)
  const preferenceZoom = useRef(defaultSettings.zoom)
  const facetAnchors = useRef<Partial<Record<keyof Selection, string>>>({})
  const typeahead = useRef({ buffer: '', timer: 0 })
  const infoGeneration = useRef(0)
  const ratingGenerations = useRef(new Map<string, { current: number }>())
  const browseKey = browseRequestKey(state.source, state.sel, state.query, state.scope)
  const view = browseViewForRequest(state.view, state.viewKey, browseKey)
  const tracks = view?.tracks ?? emptyTracks
  const displayedTracks = useMemo(() => state.settings.sortColumn
    ? [...tracks].sort((left, right) => compareTracks(left, right, state.settings.sortColumn!, state.settings.sortDesc))
    : tracks, [state.settings.sortColumn, state.settings.sortDesc, tracks])
  const selectedTracks = displayedTracks.filter((track) => state.selectedTrackIds.has(track.id))
  const spotifySearchActive = state.scope === 'spotify' && Boolean(state.query.trim() || state.spotifyNavigation)
  const tracklistVisible = !spotifySearchActive && !state.selectedPlaylist
  const libraryEmpty = view?.counts.perSource[state.source] === 0 && !state.syncPhase && !state.syncProgress
  const playbackTracks = state.playing?.queue ?? emptyTracks
  const player = usePlayer(state.connection.connected, state.connection.playback_authorized, state.playing, dispatch)
  const mainEventHandlers = useRef<MainEventHandlers>({
    playerState: player.onState,
    playbackAuthorizationRequired: player.onAuthorizationRequired,
    operationError: (error) => dispatch({ type: 'error', error }),
    operationRecovered: () => dispatch({ type: 'clear-error' }),
    localImportComplete: (summary) => dispatch({ type: 'importComplete', summary }),
    startupNotice: (notice) => dispatch({ type: 'notice', notice }),
  })
  mainEventHandlers.current = {
    playerState: player.onState,
    playbackAuthorizationRequired: player.onAuthorizationRequired,
    operationError: (error) => dispatch({ type: 'error', error }),
    operationRecovered: () => dispatch({ type: 'clear-error' }),
    localImportComplete: (summary) => dispatch({ type: 'importComplete', summary }),
    startupNotice: (notice) => dispatch({ type: 'notice', notice }),
  }
  const persistSettings = useCallback((patch: SettingsPatch) => {
    appGateway.updateSettings(patch).catch(fail)
  }, [fail])
  const updateSettings = useCallback((patch: SettingsPatch) => {
    dispatch({ type: 'settings', settings: patch })
    persistSettings(patch)
  }, [persistSettings])
  const addToPlaylist = useCallback((id: string, subject: PlaylistSubject) => subject.kind === 'album'
    ? spotifyGateway.addAlbumToPlaylist(id, subject.albumUri, subject.label)
    : spotifyGateway.addToPlaylist(id, subject.uris), [])
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
  const playFacet = useCallback((facet: keyof Selection, values: string[], anchor?: string) => {
    selectFacet(facet, values, anchor)
    setBrowserPlayKey(browseRequestKey(state.source, selectionAfterFacet(state.sel, facet, values), state.query, state.scope))
  }, [selectFacet, state.query, state.scope, state.sel, state.source])
  useEffect(() => {
    if (!browserPlayKey) return
    if (browserPlayKey !== browseKey) {
      setBrowserPlayKey(undefined)
      return
    }
    if (state.viewKey !== browserPlayKey) return
    setBrowserPlayKey(undefined)
    const first = displayedTracks[0]
    if (first && tracklistVisible) player.start(first.id, displayedTracks, { kind: 'library', source: state.source })
  }, [browserPlayKey, browseKey, displayedTracks, player, state.source, state.viewKey, tracklistVisible])
  const setBrowserPanes = useCallback((browserPanes: BrowserPanes) => {
    for (const facet of ['cat', 'art', 'alb'] as const) {
      if (!browserPanes[facet]) delete facetAnchors.current[facet]
    }
    dispatch({ type: 'browserPanes', browserPanes })
    persistSettings({ browserPanes })
  }, [persistSettings])
  const toggleBrowser = useCallback(() => {
    updateSettings({ browserVisible: !state.settings.browserVisible })
  }, [state.settings.browserVisible, updateSettings])
  const toggleBrowserPane = useCallback((facet: keyof BrowserPanes) => {
    const browserPanes = { ...state.settings.browserPanes, [facet]: !state.settings.browserPanes[facet] }
    setBrowserPanes(browserPanes)
    if (!browserPanes[facet]) setActivePane('track')
  }, [setBrowserPanes, state.settings.browserPanes])
  const openInfo = (id?: number) => {
    cancelTrackInfoLoad(infoGeneration)
    if (selectedTracks.length > 1) {
      dispatch({ type: 'info', info: { kind: 'multiple', tracks: selectedTracks } })
      return
    }
    const target = id ?? selectedTracks[0]?.id
    if (target === undefined) return
    dispatch({ type: 'info' })
    void loadCurrentGeneration(infoGeneration,
      () => libraryGateway.getTrack(target),
      (track) => dispatch({ type: 'info', info: { kind: 'single', track } }),
      fail)
  }
  const closeInfo = () => {
    cancelTrackInfoLoad(infoGeneration)
    dispatch({ type: 'info' })
  }

  useEffect(() => {
    let active = true
    const requestKey = browseRequestKey(state.source, state.sel, state.query, state.scope)
    libraryGateway.browse(state.source, state.sel, state.scope === 'library' && state.query.trim() ? state.query : undefined).then((next) => {
      if (!active) return
      const fallback = staleSelectionFacet(state.sel, next.facets)
      if (fallback) {
        selectFacet(fallback, [])
        return
      }
      dispatch({ type: 'view', view: next, key: requestKey })
    })
      .catch((error) => active && fail(error))
    return () => { active = false }
  }, [state.source, state.sel, state.query, state.scope, state.revision, fail, selectFacet])

  useEffect(() => {
    let active = true
    spotifyGateway.playlists()
      .then((rows) => active && setPlaylists(rows))
      .catch((error) => active && fail(error))
    return () => { active = false }
  }, [state.playlistRevision, fail])

  useEffect(() => {
    if (playlists && state.selectedPlaylist && !playlists.some((playlist) => playlist.id === state.selectedPlaylist)) {
      dispatch({ type: 'source', source: 'music' })
    }
  }, [playlists, state.selectedPlaylist])

  useTauriInvalidationSnapshot('settings-changed', appGateway.settings,
    (settings) => dispatch({ type: 'hydrateSettings', settings }))
  useTauriInvalidationSnapshot(spotifyEvents.connectionChanged, spotifyGateway.connectionState,
    (connection) => dispatch({ type: 'connection', connection }))
  useTauriInvalidationSnapshot('lastfm-changed', lastfmGateway.accountState,
    (lastfm) => dispatch({ type: 'lastfm', lastfm }))
  useTauriInvalidationSnapshot('lastfm-import-changed', lastfmGateway.state,
    (lastfmImport) => dispatch({ type: 'lastfmImport', lastfmImport }))

  useEffect(() => subscribeMainEvents(
    (event) => dispatchMainEvent(event, mainEventHandlers.current),
    fail,
  ), [fail])

  useTauriEvent('get-info', () => openInfo())
  useTauriEvent(libraryEvents.changed, () => dispatch({ type: 'refresh' }))
  useTauriEvent<string>(spotifyEvents.syncProgress, (phase) => dispatch({ type: 'syncPhase', phase: phase || undefined }))
  useTauriEvent<{ tracks: number; fraction: number }>(spotifyEvents.syncProgressCount, (progress) => dispatch({ type: 'syncProgress', progress }))
  useTauriEvent(spotifyEvents.playlistsChanged, () => dispatch({ type: 'playlistsRefresh' }))
  useTauriEvent(libraryEvents.localImportStarted, () => dispatch({ type: 'importStarted' }))
  useTauriEvent(libraryEvents.localImportFailed, () => dispatch({ type: 'importFailed' }))
  useTauriEvent<boolean>(libraryEvents.localDragChanged, setNativeDragActive)

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
      spotifyGateway.search(query, 0)
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
    const tick = simulatedPlaybackTick(state.playing?.simulated, state.playing?.isPlaying, state.playing?.trackId, playbackTracks)
    if (!tick) return
    const timer = window.setInterval(() => {
      dispatch({ type: 'tick', ...tick })
    }, 1000)
    return () => window.clearInterval(timer)
  }, [state.playing?.trackId, state.playing?.isPlaying, state.playing?.simulated, playbackTracks])

  const mutate = (mutation: () => Promise<unknown>) => {
    mutation()
      .then(() => dispatch({ type: 'refresh' }))
      .catch(fail)
  }
  const rate = (key: string, mutation: () => Promise<unknown>) => {
    void loadCurrentGeneration(entityRequestGeneration(ratingGenerations.current, key), mutation, () => dispatch({ type: 'refresh' }), fail)
  }
  const navigateSpotify = (track: Track, destination: 'album' | 'artist') => spotifyGateway.resolveTrackDestination(track.uri, destination)
    .then((entry) => dispatch({ type: 'spotifyNavigate', entry }))
    .catch(fail)
  const setZoom = useCallback((zoom: number) => {
    updateSettings({ zoom: normalizeZoom(zoom, ZOOM_MIN, ZOOM_MAX) })
  }, [updateSettings])
  const openPreferences = () => {
    preferenceZoom.current = state.settings.zoom
    dispatch({ type: 'preferences', open: true })
  }
  const openLastfmImporter = () => {
    appGateway.openLastFmImporter().catch(fail)
  }
  const syncLastfm = () => {
    lastfmGateway.syncPlays()
      .then((lastfmImport) => dispatch({ type: 'lastfmImport', lastfmImport }))
      .catch(fail)
  }
  const cancelPreferences = () => {
    dispatch({ type: 'settings', settings: { zoom: preferenceZoom.current } })
    dispatch({ type: 'preferences', open: false })
  }
  const saveSetupClientId = async (spotifyClientId: string) => {
    if (spotifyClientId === state.settings.spotifyClientId) return
    dispatch({ type: 'settings', settings: { spotifyClientId } })
    await appGateway.updateSettings({ spotifyClientId })
  }
  const playingTrack = playbackTracks.find((track) => track.id === state.playing?.trackId)
  const selectedAlbum = state.sel.alb?.length === 1 ? state.sel.alb[0] : undefined

  useTauriEvent<string>('view-action', (payload) => {
    if (payload === 'zoom_in') setZoom(state.settings.zoom + 0.1)
    else if (payload === 'zoom_out') setZoom(state.settings.zoom - 0.1)
    else if (payload === 'actual_size') setZoom(1)
    else if (payload === 'toggle_zebra') updateSettings({ zebra: !state.settings.zebra })
    else if (payload === 'toggle_browser') toggleBrowser()
    else if (payload.startsWith('theme_')) updateSettings({ theme: payload.slice(6) as Theme })
  })
  useTauriEvent<string>(playbackEvents.action, (payload) => {
    if (payload === 'play_pause') player.toggle()
    else player.step(payload === 'previous' ? -1 : 1)
  })
  useTauriEvent('open-preferences', openPreferences)
  useTauriEvent('open-setup', () => dispatch({ type: 'setup', open: true }))

  const onKeyDown = useRef<(event: KeyboardEvent) => void>(() => {})
  onKeyDown.current = (event: KeyboardEvent) => {
    const modalOpen = Boolean(state.info || state.preferences || state.setup || state.playbackAuthorization || playlistSubject)
    if (event.key === 'Escape' && modalOpen) {
      event.preventDefault()
      if (state.info) closeInfo()
      else if (state.preferences) cancelPreferences()
      else if (state.setup) dispatch({ type: 'setup', open: false })
      else if (state.playbackAuthorization) player.cancelPending()
      else setPlaylistSubject(undefined)
      return
    }
    const command = event.metaKey || event.ctrlKey
    if (modalOpen || !routeGlobalShortcut(event)) return
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
        const facetValues = view?.facets[activePane === 'cat' ? 'cats' : activePane === 'art' ? 'arts' : 'albs'] ?? []
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
        const facetValues = view?.facets[activePane === 'cat' ? 'cats' : activePane === 'art' ? 'arts' : 'albs'] ?? []
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
      if (!(event.metaKey || event.ctrlKey) || state.info || state.preferences || state.setup || state.playbackAuthorization || playlistSubject) return
      event.preventDefault()
      setZoom(state.settings.zoom + (event.deltaY < 0 ? 0.1 : -0.1))
    }
    window.addEventListener('wheel', onWheel, { passive: false })
    return () => window.removeEventListener('wheel', onWheel)
  }, [playlistSubject, setZoom, state.info, state.playbackAuthorization, state.preferences, state.setup, state.settings.zoom])

  const selectedPlaylist = playlists?.find((playlist) => playlist.id === state.selectedPlaylist)
  const playlistLayout = playlistLayoutFor(selectedPlaylist?.id, state.settings)
  const showPlayingOrigin = () => {
    const origin = state.playing?.origin
    if (!origin) return
    facetAnchors.current = {}
    dispatch({ type: 'scope', scope: 'library' })
    dispatch(playbackOriginAction(origin))
  }

  return (
    <main className={`app-shell ${state.settings.zebra ? 'zebra' : ''}`} style={{ zoom: appliedZoom(state.settings.zoom, ZOOM_BASE) }}>
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
        onOrigin={showPlayingOrigin}
        onArtwork={() => setArtworkOpen(true)}
      />
      <div className="body-grid">
        <Sidebar
          state={{ ...state, view }}
          playlists={playlists}
          onSource={(source) => { facetAnchors.current = {}; dispatch({ type: 'source', source }) }}
          onPlaylist={(id) => dispatch({ type: 'playlist', id })}
          onReorder={setPlaylists}
          onCollapse={() => updateSettings({ plCollapsed: !state.settings.plCollapsed })}
          onShuffle={(shuffle) => playbackGateway.setShuffle(shuffle).then(() => dispatch({ type: 'settings', settings: { shuffle } })).catch(fail)}
          onRepeat={(repeat) => playbackGateway.setRepeat(repeat).then(() => dispatch({ type: 'settings', settings: { repeat } })).catch(fail)}
          onDrop={(id, subject) => addToPlaylist(id, subject).catch(fail)}
          onError={(error) => dispatch({ type: 'error', error })}
          artwork={artworkOpen && state.playing?.uri ? <ArtworkPanel
            uri={state.playing.uri}
            name={state.playing.external ? state.playing.name ?? 'Now Playing' : playingTrack?.name ?? 'Now Playing'}
            onClose={() => setArtworkOpen(false)}
          /> : undefined}
        />
        <section className="content">
          {state.connection.needs_reauth && <div className="startup-notice reauth-notice"><span>Spotify needs to be reconnected to enable playlists.</span><button onClick={() => spotifyGateway.connect().catch(fail)}>Reconnect</button></div>}
          {spotifySearchActive ? (
            !state.connectionHydrated ? <div className="spotify-stub"><span>Checking Spotify connection…</span></div> : state.connection.connected ? <SpotifySearch
              key={JSON.stringify([state.query.trim(), state.spotifyNavigation ?? null])}
              query={state.query.trim()}
              searching={state.spotifySearching}
              results={state.spotifyResults}
              navigation={state.spotifyNavigation}
              playingUri={state.playing?.uri ?? null}
              onAdd={(album) => spotifyGateway.addAlbum(album)
                .catch((error) => { fail(error); throw error })}
              onAddTrack={(uri) => spotifyGateway.addTrack(uri)
                .catch((error) => { fail(error); throw error })}
              onRemoveTrack={(uri) => spotifyGateway.removeTrack(uri)
                .catch((error) => { fail(error); throw error })}
              onPlay={player.start}
              onPlaylist={setPlaylistSubject}
              onClose={() => dispatch({ type: 'scope', scope: 'library' })}
              onError={(error) => dispatch({ type: 'error', error })}
            /> : <div className="spotify-stub"><span>Connect to Spotify to search artists and albums.</span><button onClick={() => spotifyGateway.connect().catch(fail)}>Connect to Spotify</button></div>
          ) : selectedPlaylist ? <PlaylistView
            key={selectedPlaylist.id}
            playlist={selectedPlaylist}
            backLabel={labels[state.source].name}
            revision={state.playlistRevision}
            libraryRevision={state.revision}
            playing={state.playing}
            columnOrder={playlistLayout.columnOrder}
            columnWidths={playlistLayout.columnWidths}
            hiddenColumns={playlistLayout.hiddenColumns}
            onBack={() => dispatch({ type: 'playlist' })}
            onPlay={(id, tracks) => player.start(id, tracks, { kind: 'playlist', id: selectedPlaylist.id })}
            onRate={(id, stars) => rate(`track:${id}`, () => libraryGateway.clickTrackStar(id, stars))}
            onOpen={(target) => spotifyGateway.openPlaylist(selectedPlaylist.id, target).catch(fail)}
            onPlaylist={setPlaylistSubject}
            onInfo={(tracks) => {
              if (tracks.length > 1 || tracks[0]?.id === null) {
                cancelTrackInfoLoad(infoGeneration)
                dispatch({ type: 'info', info: { kind: 'multiple', tracks } })
              }
              else openInfo(tracks[0]?.id)
            }}
            onReorder={(columnOrder) => updateSettings({
              playlistColumnOrders: playlistOverride(state.settings.playlistColumnOrders, selectedPlaylist.id, columnOrder, PLAYLIST_DEFAULT_COLUMN_ORDER),
            })}
            onColumnWidths={(columnWidths) => updateSettings({
              playlistColumnWidths: playlistOverride(state.settings.playlistColumnWidths, selectedPlaylist.id, columnWidths, {}),
            })}
            onHiddenColumns={(hiddenColumns) => updateSettings({
              playlistHiddenColumns: playlistOverride(state.settings.playlistHiddenColumns, selectedPlaylist.id, hiddenColumns, PLAYLIST_DEFAULT_HIDDEN_COLUMNS),
            })}
            onError={(error) => dispatch({ type: 'error', error })}
          />
          : (
            <>
              <BrowserPane state={state} anchors={facetAnchors} onActivate={setActivePane} onSelect={selectFacet} onPlay={playFacet} onToggle={toggleBrowserPane} />
              {selectedAlbum !== undefined && view && !view.albumRatingAmbiguous && view.albumRatingArtist !== null && (
                <AlbumRatingStrip
                  album={selectedAlbum}
                  rating={view?.albumRating ?? null}
                  onRate={(stars) => rate(`album:${state.source}:${view.albumRatingArtist}:${selectedAlbum}`, () => libraryGateway.setAlbumRating(state.source, view.albumRatingArtist!, selectedAlbum, stars))}
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
                onPlay={(id) => player.start(id, displayedTracks, { kind: 'library', source: state.source })}
                onEnabled={(id, enabled) => mutate(() => libraryGateway.setTrackEnabled(id, enabled))}
                onRate={(id, stars) => rate(`track:${id}`, () => libraryGateway.clickTrackStar(id, stars))}
                onInfo={openInfo}
                onPlaylist={setPlaylistSubject}
                onGoToAlbum={(track) => navigateSpotify(track, 'album')}
                onGoToArtist={(track) => navigateSpotify(track, 'artist')}
                onReorder={(columnOrder) => updateSettings({ columnOrder })}
                onColumnWidths={(columnWidths) => updateSettings({ columnWidths })}
                onHiddenColumns={(hiddenColumns) => updateSettings({ hiddenColumns })}
                onSort={(sortColumn, sortDesc) => updateSettings({ sortColumn, sortDesc })}
              />
            </>
          )}
          {state.error && <div className="error-banner">{state.error}</div>}
          <StatusBar view={view} unit={labels[state.source].item} syncPhase={state.syncPhase} syncProgress={state.syncProgress} importStatus={state.importStatus} lastfmImport={state.lastfmImport} lastfmRemaining={Math.max(state.lastfmImport.remaining, state.lastfmImport.pendingReview)} onLastfmImport={openLastfmImporter} empty={libraryEmpty} />
        </section>
      </div>
      {state.info?.kind === 'single' && <GetInfo key={state.info.track.id} track={state.info.track} onCancel={closeInfo} onSaved={() => {
        closeInfo()
        dispatch({ type: 'refresh' })
      }} onError={(error) => dispatch({ type: 'error', error })} />}
      {state.info?.kind === 'multiple' && <MultipleItemInformation tracks={state.info.tracks} onCancel={closeInfo} onSaved={closeInfo} onError={(error) => dispatch({ type: 'error', error })} />}
      {state.setup && <SetupLibrary settings={state.settings} connected={state.connection.connected} connectionHydrated={state.connectionHydrated} onCancel={() => dispatch({ type: 'setup', open: false })} onConnect={(clientId) => saveSetupClientId(clientId)
        .then(spotifyGateway.connect)
        .catch(fail)} onSync={(clientId) => saveSetupClientId(clientId)
        .then(() => {
          dispatch({ type: 'setup', open: false })
          return spotifyGateway.sync()
        })
        .catch(fail)} />}
      {state.preferences && <Preferences settings={state.settings} lastfm={state.lastfm} lastfmImport={state.lastfmImport} onZoom={(zoom) => dispatch({ type: 'settings', settings: { zoom } })} onCancel={cancelPreferences} onLastfm={(lastfm) => dispatch({ type: 'lastfm', lastfm })} onImport={openLastfmImporter} onSyncLastfm={syncLastfm} onSave={({ browserPanes, ...settings }) => {
        const patch = { ...settings, browserPanes, zoom: state.settings.zoom }
        dispatch({ type: 'settings', settings })
        dispatch({ type: 'browserPanes', browserPanes })
        persistSettings(patch)
        dispatch({ type: 'preferences', open: false })
      }} />}
      {state.playbackAuthorization && <PlaybackAuthorization prompt={state.playbackAuthorization} onCancel={player.cancelPending} onAuthorize={spotifyGateway.authorizePlayback} />}
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

function useArtwork(uri: string | null | undefined, minWidth: number) {
  const [artwork, setArtwork] = useState<string | null>(null)
  useEffect(() => {
    let current = true
    if (!uri) {
      setArtwork(null)
      return () => { current = false }
    }
    setArtwork(null)
    loadArtwork(() => spotifyGateway.artwork(uri, minWidth))
      .then((url) => {
        if (current) setArtwork(url)
      })
    return () => { current = false }
  }, [minWidth, uri])
  return artwork
}

function ArtworkPanel({ uri, name, onClose }: { uri: string; name: string; onClose: () => void }) {
  const [expanded, setExpanded] = useState(false)
  const artwork = useArtwork(uri, 300)
  return <>
    <div className="sidebar-artwork">
      <button type="button" className="sidebar-artwork-close" aria-label="Hide album artwork" onClick={onClose}>×</button>
      <button type="button" className="sidebar-artwork-open" aria-label={`Enlarge artwork for ${name}`} disabled={!artwork} onClick={() => setExpanded(true)}>
        {artwork ? <img src={artwork} alt={`${name} album artwork`} /> : <span aria-hidden="true">♪</span>}
      </button>
    </div>
    {expanded && <ArtworkLightbox uri={uri} name={name} onClose={() => setExpanded(false)} />}
  </>
}

function ArtworkLightbox({ uri, name, onClose }: { uri: string; name: string; onClose: () => void }) {
  const artwork = useArtwork(uri, 640)
  return <ModalDialog className="artwork-lightbox" labelledBy="artwork-lightbox-title" onCancel={onClose} closeOnBackdrop>
    <h2 id="artwork-lightbox-title" className="visually-hidden">Artwork for {name}</h2>
    <button type="button" className="artwork-lightbox-close" aria-label="Close artwork" onClick={onClose}>×</button>
    {artwork ? <img src={artwork} alt={`${name} album artwork`} /> : <span className="artwork-placeholder" aria-hidden="true">♪</span>}
  </ModalDialog>
}

export function TransportBar({ playing, track, query, scope, volume, searchRef, onQuery, onScope, onPlay, onPrev, onNext, onVolume, onSeek, onOrigin, onArtwork }: {
  playing: State['playing']; track?: PlaybackTrack; query: string; scope: State['scope']
  volume: number
  searchRef: React.RefObject<HTMLInputElement | null>
  onQuery: (query: string) => void; onScope: (scope: State['scope']) => void; onSeek: (seconds: number) => void
  onPlay: () => void; onPrev: () => void; onNext: () => void; onVolume: (volume: number) => void; onOrigin: () => void; onArtwork: () => void
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
  const artwork = useArtwork(uri, 64)
  return <header className="transport">
    <div className="transport-controls">
      <div className="transport-buttons">
        <button aria-label="Previous track" onClick={onPrev}>⏮</button>
        <button className="play-button" aria-label={playing?.isPlaying ? 'Pause' : 'Play'} onClick={onPlay}>{playing?.isPlaying ? '⏸' : '▶'}</button>
        <button aria-label="Next track" onClick={onNext}>⏭</button>
      </div>
      <label className="volume-control"><span aria-hidden="true">−</span><input aria-label="Volume" type="range" min="0" max="100" value={volume} style={{ '--volume': `${volume}%` } as React.CSSProperties} onChange={(event) => onVolume(Number(event.target.value))} /><span aria-hidden="true">+</span></label>
    </div>
    <div
      className={`lcd ${playing?.external ? 'external' : ''} ${shown ? '' : 'idle'} ${playing?.origin ? 'returnable' : ''}`}
      role={playing?.origin ? 'button' : undefined}
      tabIndex={playing?.origin ? 0 : undefined}
      aria-label={playing?.origin ? 'Show playing source' : undefined}
      title={playing?.origin ? 'Show playing source' : undefined}
      onClick={(event) => {
        if (!playing?.origin || (event.target as Element).closest('input')) return
        onOrigin()
      }}
      onKeyDown={(event) => {
        if (!playing?.origin || (event.key !== 'Enter' && event.key !== ' ')) return
        event.preventDefault()
        onOrigin()
      }}
    >
      <button type="button" className="lcd-artwork" aria-label="Show album artwork" disabled={!uri} onClick={(event) => { event.stopPropagation(); onArtwork() }}>{artwork ? <img src={artwork} alt="" /> : <span aria-hidden="true">♪</span>}</button>
      <div className="lcd-copy">
        <Marquee text={shown?.name ?? 'Retune'} strong />
        <div className="lcd-meta">{shown ? <><span className="lcd-artist">{shown.art}</span><span className="lcd-album"> · {shown.alb}</span></> : 'Not Playing'}</div>
        <div className="progress-row"><time>{shown ? formatTime(elapsed) : '—:—'}</time><input
          aria-label="Playback position"
          type="range"
          min="0"
          max={duration || 1}
          value={Math.min(elapsed, duration || 1)}
          disabled={!shown || !duration}
          style={{ '--progress': `${duration ? Math.min(100, elapsed / duration * 100) : 0}%` } as React.CSSProperties}
          onInput={(event) => onSeek(Number(event.currentTarget.value))}
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

function Sidebar({ state, playlists, onSource, onPlaylist, onReorder, onCollapse, onShuffle, onRepeat, onDrop, onError, artwork }: {
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
  artwork?: React.ReactNode
}) {
  const [creating, setCreating] = useState(false)
  const [name, setName] = useState('')
  const [dropTarget, setDropTarget] = useState<string>()
  const [dragging, setDragging] = useState<string>()
  const [insertBefore, setInsertBefore] = useState<number>()
  const [menu, setMenu] = useState<{ x: number; y: number; playlist: PlaylistListView }>()
  const [confirming, setConfirming] = useState<PlaylistListView>()
  const [busy, setBusy] = useState(false)
  const [createBusy, setCreateBusy] = useState(false)
  const createPending = useRef(new Set<string>())
  const playlistDrag = useRef<{ id: string; pointerId: number; startY: number; moved: boolean } | undefined>(undefined)
  const dragInsertBefore = useRef<number | undefined>(undefined)
  const suppressPlaylistClick = useRef(false)
  const create = async () => {
    if (!name.trim() || !beginPendingEntity(createPending.current, 'create')) return
    setCreateBusy(true)
    try {
      await spotifyGateway.createPlaylist(name)
      setName('')
      setCreating(false)
    } catch (error) {
      onError(String(error))
    } finally {
      createPending.current.delete('create')
      setCreateBusy(false)
    }
  }
  const reorder = async (dragged: string, target: number) => {
    if (!playlists) return
    const ids = moveToIndex(playlists.map((playlist) => playlist.id), dragged, target)
    const reordered = ids.map((id) => playlists.find((playlist) => playlist.id === id)!)
    setInsertBefore(undefined)
    onReorder(reordered)
    try { await spotifyGateway.reorderPlaylists(ids) }
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
      await spotifyGateway.unfollowPlaylist(confirming.id)
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
    {!state.settings.plCollapsed && creating && <div className="playlist-new-row"><input autoFocus aria-label="Playlist name" disabled={createBusy} value={name} onChange={(event) => setName(event.target.value)} onKeyDown={(event) => {
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
    {artwork}
    <div className="sidebar-actions">
      <button data-tooltip="New playlist" aria-label="New playlist" onClick={() => { setCreating(true); if (state.settings.plCollapsed) onCollapse() }}><svg aria-hidden="true" width="14" height="14" viewBox="0 0 18 18" fill="none" stroke="currentColor" strokeWidth="1.9" strokeLinecap="round"><path d="M9 3.6v10.8M3.6 9h10.8" /></svg></button>
      <button className={state.settings.shuffle ? 'active' : ''} data-tooltip={`Shuffle: ${state.settings.shuffle ? 'on' : 'off'}`} aria-label={`Shuffle: ${state.settings.shuffle ? 'on' : 'off'}`} aria-pressed={state.settings.shuffle} onClick={() => onShuffle(!state.settings.shuffle)}><svg aria-hidden="true" width="15" height="15" viewBox="0 0 18 18" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round"><path d="M2.6 5h3.2C7 5 7.9 6.4 9 9s2 4 3.2 4h2.6M2.6 13h3.2C7 13 7.9 11.6 9 9s2-4 3.2-4h2.6M12.9 3.1 15.1 5l-2.2 1.9M12.9 11.1l2.2 1.9-2.2 1.9" /></svg></button>
      <button className={state.settings.repeat !== 'off' ? 'active' : ''} data-tooltip={`Repeat: ${state.settings.repeat}`} aria-label={`Repeat: ${state.settings.repeat}`} aria-pressed={state.settings.repeat !== 'off'} onClick={() => onRepeat(state.settings.repeat === 'off' ? 'all' : state.settings.repeat === 'all' ? 'one' : 'off')}><svg aria-hidden="true" width="15" height="15" viewBox="0 0 18 18" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round"><path d="M3.9 9.4V7.2a3 3 0 0 1 3-3h6.5M11.6 2.4l2.1 1.8L11.6 6M14.1 8.6v2.2a3 3 0 0 1-3 3H4.6M6.4 12l-2.1 1.8 2.1 1.8" /></svg>{state.settings.repeat === 'one' && <span className="repeat-one-badge" aria-hidden="true">1</span>}</button>
    </div>
  </aside>{confirming && <ModalDialog className="get-info playlist-confirm" labelledBy="playlist-confirm-title" onCancel={busy ? undefined : () => setConfirming(undefined)} onSubmit={busy ? undefined : unfollow} closeOnBackdrop><h2 id="playlist-confirm-title">{confirming.owned ? 'Delete Playlist?' : 'Unfollow Playlist?'}</h2><p>{confirming.owned ? `Delete “${confirming.name}” from Spotify?` : `Stop following “${confirming.name}”?`}</p><div className="modal-actions"><button type="button" autoFocus disabled={busy} onClick={() => setConfirming(undefined)}>Cancel</button><button type="submit" className="danger" disabled={busy}>{busy ? 'Working…' : confirming.owned ? 'Delete' : 'Unfollow'}</button></div></ModalDialog>}</>
}

function PlaylistView({ playlist, backLabel, revision, libraryRevision, playing, columnOrder, columnWidths, hiddenColumns, onBack, onPlay, onRate, onOpen, onPlaylist, onInfo, onReorder, onColumnWidths, onHiddenColumns, onError }: {
  playlist: PlaylistListView
  backLabel: string
  revision: number
  libraryRevision: number
  playing: State['playing']
  columnOrder: ColumnKey[]
  columnWidths: Partial<Record<ColumnKey, number>>
  hiddenColumns: ColumnKey[]
  onBack: () => void
  onPlay: (id: number, tracks: readonly PlaybackTrack[]) => void
  onRate: (id: number, stars: number) => void
  onOpen: (target: 'app' | 'web') => void
  onPlaylist: (subject: PlaylistSubject) => void
  onInfo: (tracks: PlaylistTrack[]) => void
  onReorder: (order: ColumnKey[]) => void
  onColumnWidths: (widths: Partial<Record<ColumnKey, number>>) => void
  onHiddenColumns: (columns: ColumnKey[]) => void
  onError: (error: string) => void
}) {
  const [trackState, setTrackState] = useState(() => loadingPlaylistRows<PlaylistTrack>(playlist.id))
  const [selected, setSelected] = useState<Set<number>>(new Set())
  const [selectionAnchor, setSelectionAnchor] = useState<number>()
  const [focusedUpstreamIndex, setFocusedUpstreamIndex] = useState<number>()
  const [insertBefore, setInsertBefore] = useState<number>()
  const [mutating, setMutating] = useState(false)
  const [sortColumn, setSortColumn] = useState<ColumnKey | null>(null)
  const [sortDesc, setSortDesc] = useState(false)
  const [liveWidths, setLiveWidths] = useState(columnWidths)
  const [menu, setMenu] = useState<{ x: number; y: number; upstreamIndex?: number }>()
  const onErrorRef = useRef(onError)
  const trackDrag = useRef<{ indices: number[]; pointerId: number; startY: number; moved: boolean } | undefined>(undefined)
  const dragInsertBefore = useRef<number | undefined>(undefined)
  const suppressTrackClick = useRef(false)
  const headerDragged = useRef(false)
  const columnDrag = useRef<{ column: ColumnKey; pointerId: number; startX: number; element: HTMLButtonElement } | undefined>(undefined)
  const resize = useRef<{ column: ColumnKey; pointerId: number; startX: number; startWidth: number } | undefined>(undefined)
  onErrorRef.current = onError
  const tracks = currentPlaylistRows(trackState, playlist.id)
  const canChangePlaylist = playlist.owned && playlistRowsReady(trackState, playlist.id) && tracks.length === playlist.trackCount
  const canReorder = canChangePlaylist && sortColumn === null
  useEffect(() => setLiveWidths(columnWidths), [columnWidths])
  useEffect(() => {
    setTrackState(loadingPlaylistRows(playlist.id))
    setSelected(new Set())
    setSelectionAnchor(undefined)
    setFocusedUpstreamIndex(undefined)
    if (!playlist.itemsAvailable) {
      return
    }
    let active = true
    spotifyGateway.playlistTracks(playlist.id)
      .then((rows) => active && setTrackState((current) => resolvedPlaylistRows(current, playlist.id, rows)))
      .catch((error) => {
        if (!active) return
        setTrackState((current) => failedPlaylistRows(current, playlist.id))
        onErrorRef.current(String(error))
      })
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
  const rovingUpstreamIndex = rows.some((row) => row.upstreamIndex === focusedUpstreamIndex)
    ? focusedUpstreamIndex
    : rows.find((row) => selected.has(row.upstreamIndex))?.upstreamIndex ?? rows[0]?.upstreamIndex
  const queue: PlaybackTrack[] = rows.map(({ track, upstreamIndex }) => ({ ...track, id: track.id ?? SYNTHETIC_BASE + upstreamIndex }))
  const headings = { ...trackColumnHeadings(labels.music), track: 'Track' }
  const customizableColumns = columnOrder.filter((column) => PLAYLIST_COLUMNS.includes(column))
  const visibleColumns = visibleColumnOrder(customizableColumns, hiddenColumns)
  const columns = trackGridColumns(visibleColumns, liveWidths, '35px')
  const moveColumn = (event: React.PointerEvent<HTMLButtonElement>) => {
    const active = columnDrag.current
    if (!active || active.pointerId !== event.pointerId) return
    if (Math.abs(event.clientX - active.startX) > 4) {
      headerDragged.current = true
      active.element.classList.add('dragging')
    }
  }
  const endColumn = (event: React.PointerEvent<HTMLButtonElement>) => {
    const active = columnDrag.current
    if (!active || active.pointerId !== event.pointerId) return
    active.element.classList.remove('dragging')
    columnDrag.current = undefined
    if (!headerDragged.current) return
    const target = visibleColumns.find((column) => {
      const element = event.currentTarget.parentElement?.querySelector<HTMLElement>(`[data-column="${column}"]`)
      return element != null && event.clientX < element.getBoundingClientRect().left + element.getBoundingClientRect().width / 2
    })
    onReorder(moveBefore(columnOrder, active.column, target))
  }
  const beginResize = (event: React.PointerEvent<HTMLSpanElement>, column: ColumnKey) => {
    event.preventDefault()
    event.stopPropagation()
    headerDragged.current = true
    event.currentTarget.setPointerCapture(event.pointerId)
    resize.current = { column, pointerId: event.pointerId, startX: event.clientX, startWidth: event.currentTarget.parentElement?.getBoundingClientRect().width ?? 60 }
  }
  const moveResize = (event: React.PointerEvent<HTMLSpanElement>) => {
    const active = resize.current
    if (!active || active.pointerId !== event.pointerId) return
    setLiveWidths((widths) => ({ ...widths, [active.column]: resizedColumnWidth(active.startWidth, active.startX, event.clientX) }))
  }
  const endResize = (event: React.PointerEvent<HTMLSpanElement>) => {
    const active = resize.current
    if (!active || active.pointerId !== event.pointerId) return
    const width = resizedColumnWidth(active.startWidth, active.startX, event.clientX)
    resize.current = undefined
    onColumnWidths({ ...columnWidths, [active.column]: width })
  }
  const cancelResize = (event: React.PointerEvent<HTMLSpanElement>) => {
    if (resize.current?.pointerId !== event.pointerId) return
    resize.current = undefined
    setLiveWidths(columnWidths)
  }
  const select = (upstreamIndex: number, event: Pick<React.MouseEvent | React.KeyboardEvent, 'shiftKey' | 'metaKey' | 'ctrlKey'>) => {
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
  const reorder = async (range: { start: number; length: number }, index: number) => {
    if (index >= range.start && index <= range.start + range.length) {
      setInsertBefore(undefined)
      return
    }
    setMutating(true)
    setInsertBefore(undefined)
    try {
      await spotifyGateway.reorderPlaylist(playlist.id, range.start, index, range.length)
    } catch (error) {
      onError(String(error))
    } finally {
      setSelected(new Set())
      setSelectionAnchor(undefined)
      setMutating(false)
    }
  }
  const cancelTrackDrag = () => {
    trackDrag.current = undefined
    dragInsertBefore.current = undefined
    setInsertBefore(undefined)
  }
  const remove = async (target?: number) => {
    const indices = selectedIndices(target)
    if (!indices.length || !window.confirm(`Remove ${indices.length} selected ${indices.length === 1 ? 'track' : 'tracks'} from “${playlist.name}”?`)) return
    setMutating(true)
    try {
      await spotifyGateway.removeFromPlaylist(playlist.id, indices)
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
      await spotifyGateway.addTracks(uris)
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
    <SpotifyPageBack label={backLabel} onBack={onBack} />
    <header className="playlist-header"><strong>{playlist.name}</strong><span>{playlist.trackCount} {playlist.trackCount === 1 ? 'track' : 'tracks'}{playlist.owner ? ` · by ${playlist.owner}` : ''}{sortColumn ? ` · sorted by ${headings[sortColumn]}` : ''}</span>{playlist.owned && <button disabled={!canChangePlaylist || !selected.size || mutating} onClick={() => void remove()}>Remove</button>}</header>
    {!playlist.itemsAvailable ? <div className="playlist-unavailable"><strong>Tracks unavailable in Retune</strong><span>Spotify does not allow third-party apps to interact with playlists not owned by you. :-(</span><div className="playlist-open-actions"><button onClick={() => onOpen('app')}>Open in Spotify app</button><button onClick={() => onOpen('web')}>Open on Spotify Web</button></div></div> : <>
    <div className="playlist-track-scroll" aria-label={`${playlist.name} tracks`} onClick={(event) => {
      if (event.target !== event.currentTarget && !(event.target as Element).closest('.playlist-end-drop')) return
      setSelected(new Set())
      setSelectionAnchor(undefined)
    }}>
      <div className="playlist-track-header track-header" style={{ gridTemplateColumns: columns }} onContextMenu={(event) => { event.preventDefault(); setMenu({ x: event.clientX, y: event.clientY }) }}>
        <button type="button" aria-label="Restore Spotify playlist order" title="Spotify order · click to restore" className={sortColumn === null ? 'active' : ''} onClick={() => { setSortColumn(null); setSortDesc(false) }}>#</button>
        {visibleColumns.map((column) => <button type="button" key={column} data-column={column} className={COLUMN_SPECS[column].numeric ? 'track-number' : ''} onPointerDown={(event) => {
          if (event.button !== 0) return
          headerDragged.current = false
          columnDrag.current = { column, pointerId: event.pointerId, startX: event.clientX, element: event.currentTarget }
          event.currentTarget.setPointerCapture(event.pointerId)
        }} onPointerMove={moveColumn} onPointerUp={endColumn} onPointerCancel={() => {
          columnDrag.current?.element.classList.remove('dragging')
          columnDrag.current = undefined
        }} onClick={() => {
          if (headerDragged.current) return
          setSortDesc(sortColumn === column ? !sortDesc : false)
          setSortColumn(column)
        }}><span className="track-header-label">{headings[column]}{sortColumn === column ? sortDesc ? ' ▼' : ' ▲' : ''}</span><span className="column-resize-handle" draggable={false} onPointerDown={(event) => beginResize(event, column)} onPointerMove={moveResize} onPointerUp={endResize} onPointerCancel={cancelResize} onClick={(event) => {
          event.preventDefault()
          event.stopPropagation()
        }} onDragStart={(event) => {
          event.preventDefault()
          event.stopPropagation()
        }} /></button>)}
      </div>
      {rows.map(({ track, upstreamIndex }, rowIndex) => <div
        key={`${track.uri}-${upstreamIndex}`}
        role="group"
        aria-label={`${track.name} by ${track.art}`}
        data-keyboard-row
        tabIndex={rovingUpstreamIndex === upstreamIndex ? 0 : -1}
        className={`playlist-track-row track-row ${canReorder && !mutating ? 'reorderable' : ''} ${selected.has(upstreamIndex) ? 'selected' : ''} ${insertBefore === upstreamIndex ? 'insert-before' : ''} ${isCurrentTrack(playing, queue[rowIndex]) ? 'playing' : ''}`}
        style={{ gridTemplateColumns: columns }}
        onFocus={() => setFocusedUpstreamIndex(upstreamIndex)}
        onClick={(event) => {
          if (suppressTrackClick.current) { suppressTrackClick.current = false; event.preventDefault(); return }
          select(upstreamIndex, event)
        }}
        onDoubleClick={() => onPlay(queue[rowIndex].id, queue)}
        onKeyDown={(event) => {
          if (event.target !== event.currentTarget) return
          if (event.key === 'Enter') {
            event.preventDefault()
            onPlay(queue[rowIndex].id, queue)
            return
          }
          if (event.key === ' ') {
            event.preventDefault()
            select(upstreamIndex, event)
            return
          }
          if (!['ArrowUp', 'ArrowDown', 'Home', 'End'].includes(event.key)) return
          event.preventDefault()
          const nextIndex = event.key === 'Home' ? 0 : event.key === 'End' ? rows.length - 1 : Math.max(0, Math.min(rows.length - 1, rowIndex + (event.key === 'ArrowUp' ? -1 : 1)))
          const next = rows[nextIndex]
          if (!next) return
          select(next.upstreamIndex, event)
          const rowContainer = event.currentTarget.parentElement
          window.requestAnimationFrame(() => rowContainer?.querySelectorAll<HTMLElement>('.playlist-track-row')[nextIndex]?.focus())
        }}
        onPointerDown={canReorder && !mutating ? (event) => {
          if (event.button !== 0) return
          suppressTrackClick.current = false
          trackDrag.current = { indices: selected.has(upstreamIndex) ? [...selected] : [upstreamIndex], pointerId: event.pointerId, startY: event.clientY, moved: false }
          event.currentTarget.setPointerCapture(event.pointerId)
        } : undefined}
        onPointerMove={canReorder && !mutating ? (event) => {
          const drag = trackDrag.current
          if (!drag || drag.pointerId !== event.pointerId || (!drag.moved && Math.abs(event.clientY - drag.startY) < 4)) return
          if (!contiguousRange(drag.indices)) {
            cancelTrackDrag()
            onError('Select a contiguous block of tracks to reorder.')
            return
          }
          drag.moved = true
          event.preventDefault()
          const trackRows = [...event.currentTarget.parentElement!.querySelectorAll<HTMLElement>('.playlist-track-row')]
          const target = insertionIndexAtY(trackRows.map((row) => { const bounds = row.getBoundingClientRect(); return bounds.top + bounds.height / 2 }), event.clientY)
          dragInsertBefore.current = target
          setInsertBefore(target)
        } : undefined}
        onPointerUp={canReorder && !mutating ? (event) => {
          const drag = trackDrag.current
          if (!drag || drag.pointerId !== event.pointerId) return
          const range = contiguousRange(drag.indices)
          const target = dragInsertBefore.current
          const moved = drag.moved
          cancelTrackDrag()
          if (!moved || !range || target === undefined) return
          event.preventDefault()
          suppressTrackClick.current = true
          window.setTimeout(() => { suppressTrackClick.current = false }, 0)
          void reorder(range, target)
        } : undefined}
        onPointerCancel={cancelTrackDrag}
        onContextMenu={(event) => {
          event.preventDefault()
          event.currentTarget.focus()
          if (!selected.has(upstreamIndex)) select(upstreamIndex, event)
          setMenu({ x: event.clientX, y: event.clientY, upstreamIndex })
        }}
      ><span className="track-number">{upstreamIndex + 1}</span>{visibleColumns.map((column) => <TrackCell key={column} track={track} column={column} facetTitle={headings.genre} playing={isCurrentTrack(playing, queue[rowIndex]) ? playing?.isPlaying ? 'playing' : 'paused' : false} selected={selected.has(upstreamIndex)} onRate={track.id === null ? undefined : onRate.bind(null, track.id)} />)}</div>)}
      {canReorder && <div className={`playlist-end-drop ${insertBefore === tracks.length ? 'insert-before' : ''}`} />}
    </div>
    {menu && (menu.upstreamIndex === undefined
      ? <CheckboxMenu x={menu.x} y={menu.y} onClose={() => setMenu(undefined)} items={customizableColumns.map((column) => ({ key: column, label: headings[column], checked: !hiddenColumns.includes(column), disabled: column === 'name', onChange: (checked) => onHiddenColumns(checked ? hiddenColumns.filter((hidden) => hidden !== column) : [...hiddenColumns, column]) }))} />
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
  const [busy, setBusy] = useState<Set<string>>(new Set())
  const pending = useRef(new Set<string>())
  const [creating, setCreating] = useState(false)
  const [name, setName] = useState('')
  const reportError = useEffectEvent(onError)
  useEffect(() => {
    let active = true
    spotifyGateway.playlists(subject.kind === 'tracks' ? subject.uris : undefined)
      .then((rows) => active && setPlaylists(rows))
      .catch((error) => active && reportError(String(error)))
    return () => { active = false }
  }, [revision, subject])
  const add = async (id: string) => {
    if (!beginPendingEntity(pending.current, id)) return
    setBusy(pendingEntities(pending.current))
    try { await onAdd(id, subject) }
    catch (error) { onError(String(error)) }
    finally { pending.current.delete(id); setBusy(pendingEntities(pending.current)) }
  }
  const create = async () => {
    if (!name.trim() || !beginPendingEntity(pending.current, 'new')) return
    setBusy(pendingEntities(pending.current))
    try {
      const playlist = await spotifyGateway.createPlaylist(name)
      await onAdd(playlist.id, subject)
      setName('')
      setCreating(false)
    } catch (error) {
      onError(String(error))
    } finally {
      pending.current.delete('new')
      setBusy(pendingEntities(pending.current))
    }
  }
  return <ModalDialog className="playlist-popover" labelledBy="add-to-playlist-title" onCancel={onClose} closeOnBackdrop>
      <header><h2 id="add-to-playlist-title">Add to Playlist</h2><span>{subject.label}</span></header>
      {local && <p className="playlist-local-hint">{LOCAL_PLAYLIST_HINT}</p>}
      <div className="playlist-popover-list">{playlists.map((playlist) => <button type="button" key={playlist.id} disabled={local || !playlist.owned || busy.has(playlist.id)} onClick={() => void add(playlist.id)}>
        <span>{playlist.contains ? '✓' : ''}</span><span>{playlist.owned ? '' : '🌐'}</span><strong>{playlist.name}</strong>{!playlist.owned && <small>{playlist.owner}</small>}
      </button>)}</div>
      <footer>{creating
        ? <input autoFocus aria-label="Playlist name" disabled={busy.has('new')} value={name} onChange={(event) => setName(event.target.value)} onKeyDown={(event) => {
          if (event.key === 'Enter') void create()
        }} />
        : <button type="button" className="new-playlist-button" disabled={local} onClick={() => setCreating(true)}>+ New Playlist</button>}
        <button type="button" className="done-button" onClick={onClose}>Done</button></footer>
  </ModalDialog>
}

function StatusBar({ view, unit, syncPhase, syncProgress, importStatus, lastfmImport, lastfmRemaining, onLastfmImport, empty }: { view: BrowseView | null; unit: string; syncPhase?: string; syncProgress?: { tracks: number; fraction: number }; importStatus?: string; lastfmImport: LastFmImportState; lastfmRemaining: number; onLastfmImport: () => void; empty: boolean }) {
  const total = view?.counts.totalSecs ?? 0
  const hours = Math.floor(total / 3600)
  const minutes = Math.floor((total % 3600) / 60)
  const count = view?.counts.tracks ?? 0
  const sourceWork = lastfmImport.phase === 'downloading' || lastfmImport.phase === 'aggregating'
  const progress = importDownloadProgressLabel(lastfmImport.processedScrobbles, lastfmImport.totalScrobbles, importDownloadPercent(lastfmImport.downloadedPages, lastfmImport.totalPages))
  return <footer className="status-bar">{syncProgress
    ? <span className="sync-status"><span>⟳ Syncing from Spotify…</span><progress className="sync-meter" max={1} value={syncProgress.fraction} /><span>{syncProgress.tracks} tracks synced</span></span>
    : sourceWork ? <button type="button" className="status-import-link" onClick={onLastfmImport}>{importStatusText(lastfmImport.phase, lastfmImport.username)} · {progress}</button>
    : lastfmImport.syncing ? <button type="button" className="status-import-link" onClick={onLastfmImport}>⟳ Syncing Last.fm plays…</button>
    : lastfmImport.syncProblem ? <button type="button" className="status-import-link" onClick={onLastfmImport}>⚠ Last.fm sync needs attention</button>
    : lastfmRemaining > 0 ? <button type="button" className="status-import-link" onClick={onLastfmImport}>⚠ Finish importing from Last.fm — {lastfmRemaining} left</button>
    : <span>{syncPhase ?? importStatus ?? (empty ? 'No library — set up to begin' : `${count} ${count === 1 ? unit : `${unit}s`}, ${hours}:${String(minutes).padStart(2, '0')} hours`)}</span>}</footer>
}

export default App
