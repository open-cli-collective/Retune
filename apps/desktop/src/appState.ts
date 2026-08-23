import { LIBRARY_DEFAULT_COLUMN_ORDER, LIBRARY_DEFAULT_HIDDEN_COLUMNS, rememberSelection, restoreSelection, selectionAfterFacet } from './ui.ts'
import type { BrowseView, BrowserPanes, ConnectionState, ImportSummary, InfoDialog, LastFmImportState, LastFmState, PlaybackAuthorizationPrompt, PlaybackOrigin, PlaybackTrack, PlayerState, Playing, Selection, Settings, Source, SpotifyNavEntry, SpotifyResults } from './types.ts'

const emptyTracks: PlaybackTrack[] = []

export type State = {
  source: Source
  sel: Selection
  savedSelections: Record<Source, Selection>
  query: string
  scope: 'library' | 'spotify'
  selectedTrackIds: Set<number>
  selectionAnchor?: number
  playing: Playing | null
  settings: Settings
  settingsHydrated: boolean
  systemDark: boolean
  view: BrowseView | null
  viewKey?: string
  revision: number
  error?: string
  notice?: string
  info?: InfoDialog
  preferences: boolean
  setup: boolean
  playbackAuthorization: PlaybackAuthorizationPrompt | null
  connection: ConnectionState
  lastfm: LastFmState
  lastfmImport: LastFmImportState
  spotifyResults: SpotifyResults | null
  spotifySearching: boolean
  spotifyNavigation?: SpotifyNavEntry
  selectedPlaylist?: string
  playlistRevision: number
  syncPhase?: string
  syncProgress?: { tracks: number; fraction: number }
  importStatus?: string
}

export type Action =
  | { type: 'view'; view: BrowseView; key: string }
  | { type: 'error'; error: string }
  | { type: 'clear-error' }
  | { type: 'source'; source: Source }
  | { type: 'playlist'; id?: string }
  | { type: 'select'; facet: keyof Selection; values: string[] }
  | { type: 'query'; query: string }
  | { type: 'scope'; scope: State['scope'] }
  | { type: 'selectTrack'; id: number }
  | { type: 'selection'; ids: Set<number>; anchor?: number }
  | { type: 'play'; id: number; queue: readonly PlaybackTrack[]; origin?: PlaybackOrigin }
  | { type: 'togglePlay' }
  | { type: 'step'; id: number }
  | { type: 'tick'; duration: number; nextId: number }
  | { type: 'seek'; elapsed: number }
  | { type: 'playerState'; player: PlayerState; queue: readonly PlaybackTrack[]; origin?: PlaybackOrigin }
  | { type: 'hydrateSettings'; settings: Settings }
  | { type: 'settings'; settings: Partial<Settings> }
  | { type: 'browserPanes'; browserPanes: BrowserPanes }
  | { type: 'systemTheme'; dark: boolean }
  | { type: 'refresh' }
  | { type: 'notice'; notice?: string }
  | { type: 'info'; info?: InfoDialog }
  | { type: 'preferences'; open: boolean }
  | { type: 'setup'; open: boolean }
  | { type: 'playbackAuthorization'; prompt: PlaybackAuthorizationPrompt | null }
  | { type: 'connection'; connection: ConnectionState }
  | { type: 'lastfm'; lastfm: LastFmState }
  | { type: 'lastfmImport'; lastfmImport: LastFmImportState }
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

export const defaultSettings: Settings = {
  theme: 'system',
  zoom: 1,
  zebra: true,
  plCollapsed: false,
  browserVisible: true,
  browserPanes: { cat: true, art: true, alb: true },
  // Mirrors Settings::default() in src-tauri/src/store.rs — keep in sync
  columnOrder: [...LIBRARY_DEFAULT_COLUMN_ORDER],
  columnWidths: {},
  hiddenColumns: [...LIBRARY_DEFAULT_HIDDEN_COLUMNS],
  playlistHiddenColumns: {},
  playlistColumnOrders: {},
  playlistColumnWidths: {},
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
  lastfmScrobbling: true,
  lastfmScrobblingProfile: null,
}

export const initialState: State = {
  source: 'music',
  sel: {},
  savedSelections: { music: {}, podcasts: {}, audiobooks: {} },
  query: '',
  scope: 'library',
  selectedTrackIds: new Set(),
  playing: null,
  settings: defaultSettings,
  settingsHydrated: false,
  systemDark: false,
  view: null,
  viewKey: undefined,
  revision: 0,
  preferences: false,
  setup: false,
  playbackAuthorization: null,
  connection: { connected: false, needs_reauth: false, playback_authorized: false },
  lastfm: { available: false, connected: false, username: null, pending: false, reconnectRequired: false, problem: null },
  lastfmImport: { phase: null, username: null, spotifyAccountId: null, historyTo: null, downloadedThrough: null, nextPage: 1, totalPages: null, downloadedPages: 0, totalScrobbles: 0, includedScrobbles: 0, processedScrobbles: 0, defaults: { importContent: true, includeHistoricalPlayCounts: true, wholeAlbum: false }, remaining: 0, retryableError: null, searchTerms: true, syncing: false, lastSyncedAt: null, pendingReview: 0, syncProblem: null, applyingAll: false },
  spotifyResults: null,
  spotifySearching: false,
  playlistRevision: 0,
}

export function reducer(state: State, action: Action): State {
  switch (action.type) {
    case 'view':
      return { ...state, view: action.view, viewKey: action.key, error: undefined }
    case 'error':
      return { ...state, error: action.error, syncProgress: undefined }
    case 'clear-error':
      return { ...state, error: undefined }
    case 'source':
      return { ...state, source: action.source, sel: restoreSelection(state.savedSelections, action.source), query: '', spotifyNavigation: undefined, selectedPlaylist: undefined, selectedTrackIds: new Set(), selectionAnchor: undefined }
    case 'playlist':
      return { ...state, selectedPlaylist: action.id, spotifyNavigation: undefined, selectedTrackIds: new Set(), selectionAnchor: undefined }
    case 'select': {
      const sel = selectionAfterFacet(state.sel, action.facet, action.values)
      return { ...state, sel, savedSelections: rememberSelection(state.savedSelections, state.source, sel), selectedTrackIds: new Set(), selectionAnchor: undefined }
    }
    case 'query':
      return { ...state, query: action.query, spotifyResults: null, spotifySearching: false, spotifyNavigation: undefined, selectedTrackIds: new Set(), selectionAnchor: undefined }
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
          shuffle: state.settings.shuffle, origin: action.origin, simulated: true,
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
            playing: { ...action.player, queue: action.player.external ? emptyTracks : action.queue, origin: action.origin },
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
      return { ...state, sel, savedSelections: rememberSelection(state.savedSelections, state.source, sel), settings: { ...state.settings, browserPanes: action.browserPanes } }
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
    case 'playbackAuthorization':
      return action.prompt
        ? { ...state, playbackAuthorization: action.prompt, info: undefined, preferences: false, setup: false }
        : { ...state, playbackAuthorization: null }
    case 'connection':
      return { ...state, connection: action.connection, playbackAuthorization: action.connection.playback_authorized ? null : state.playbackAuthorization }
    case 'lastfm':
      return { ...state, lastfm: action.lastfm }
    case 'lastfmImport':
      return { ...state, lastfmImport: action.lastfmImport }
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
