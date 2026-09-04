import { tauriInvoker, type Invoker } from './ipc.ts'
import type { CountMode, ImportQueuePage, ImportSourceRow } from './lastfmImportState.ts'
import type { LastFmImportDefaults, LastFmImportState, LastFmState } from './types.ts'

export type AlbumCandidate = { uri: string; name: string; artist: string; inLibrary: boolean; relation: 'best-match' | 'same-songs' | 'superset' | null; trackUris: string[]; trackNames: string[]; trackArtists: string[]; trackAlbums: string[]; imageUrl?: string | null; releaseDate?: string | null; albumType?: string | null; totalTracks?: number; trackNumbers?: (number | null)[]; trackDurations?: number[] }
export type CollectionAlbumCoverage = { uri: string; matched: number; uniqueCoverage: number }
export type CollectionAlbumPreviewCoverage = { uri: string; selected: boolean; matched: number; uniqueCoverage: number; marginalMatches: number; ambiguityChanges: number; trackStatuses: Array<{ uri: string; status: 'matched' | 'ambiguous' | 'unmatched' }> }
export type CollectionMatchView = { cachedAlbums: AlbumCandidate[]; selectedAlbumUris: string[]; fullAlbumUris: string[]; wholeAlbumReady: boolean; coverage: { matched: number; ambiguous: number; unresolved: number; selectedAlbums: CollectionAlbumCoverage[]; previews: CollectionAlbumPreviewCoverage[] } }
export type MatchResult = { sourceId: string; searchTerm: string; confidence: 'exact' | 'likely' | 'low' | null; selectedUri: string | null; candidates: AlbumCandidate[]; trackMatches: Record<string, string> }
export type PageItem = { source: ImportSourceRow; decision: { status: 'pending' | 'done' | 'skipped' | 'ignored-album' | 'ignored-artist'; excluded: boolean }; matchResult: MatchResult | null }
export type ImportPageOptions = { importContent: boolean; includeHistoricalPlayCounts: boolean; wholeAlbum: boolean; genre: string | null; rating: number | null; selectedTrackIds: string[] }
export type PageView = { state: LastFmImportState; batchId: number; artist: string; album: string; customBatch?: boolean; collectionShaped?: boolean; albumLabelCount?: number; pageNumber: number; pageCount: number; rows: PageItem[]; options: ImportPageOptions; fuzzyGroups: Record<string, ImportSourceRow[]>; countModes: Record<string, CountMode>; resolvedCounts: Record<string, number>; lockedCountModes: string[]; collection: CollectionMatchView | null }
export type ReviewAction = 'exclude' | 'undo-exclude' | 'skip-album' | 'restore' | 'ignore-album' | 'ignore-artist'

type Batch = { batchId: number; artist: string; album: string }

export const lastfmEvents = {
  changed: 'lastfm-import-changed',
  applyFinished: 'lastfm-import-apply-finished',
} as const

export function createLastFmGateway(invoke: Invoker) {
  return {
    state: () => invoke<LastFmImportState>('lastfm_import_state'),
    queue: (cursor: number, limit: number) => invoke<ImportQueuePage>('lastfm_import_queue', { cursor, limit }),
    page: ({ batchId, artist, album }: Batch) => invoke<PageView | null>('lastfm_import_page', { batchId, artist, album }),
    combineBatches: (batchIds: number[]) => invoke<PageView | null>('lastfm_import_combine_batches', { batchIds }),
    review: ({ batchId, action, artist, album, ids }: Batch & { action: ReviewAction; ids?: string[] }) => invoke<LastFmImportState>('lastfm_import_review', { batchId, ...(ids === undefined ? {} : { ids }), action, artist, album }),
    saveOptions: ({ batchId, artist, album }: Batch, options: ImportPageOptions) => invoke<void>('lastfm_import_options', { batchId, artist, album, options }),
    apply: ({ batchId, artist, album }: Batch, selectedIds: string[], archiveBatch: boolean, options: ImportPageOptions) => invoke<void>('lastfm_import_apply', { batchId, artist, album, selectedIds, archiveBatch, options }),
    retryApply: (batchId: number) => invoke<void>('lastfm_import_retry_apply', { batchId }),
    countMode: (targetUri: string, mode: CountMode) => invoke<void>('lastfm_import_count_mode', { targetUri, mode }),
    activateCollection: ({ batchId, artist, album }: Batch) => invoke<PageView | null>('lastfm_import_activate_collection', { batchId, artist, album }),
    collectionSearchAlbums: (batchId: number, artist: string, query: string) => invoke<AlbumCandidate[]>('lastfm_import_collection_search_albums', { batchId, artist, query }),
    collectionPreviewAlbum: (batchId: number, artist: string, uri: string) => invoke<PageView | null>('lastfm_import_collection_preview_album', { batchId, artist, uri }),
    collectionAddAlbum: (batchId: number, artist: string, uri: string) => invoke<PageView | null>('lastfm_import_collection_add_album', { batchId, artist, uri }),
    collectionRemoveAlbum: (batchId: number, artist: string, uri: string) => invoke<PageView | null>('lastfm_import_collection_remove_album', { batchId, artist, uri }),
    collectionSetAlbumImport: (batchId: number, artist: string, uri: string, enabled: boolean) => invoke<PageView | null>('lastfm_import_collection_set_album_import', { batchId, artist, uri, enabled }),
    changeTrack: (batchId: number, id: string, query: string) => invoke<PageView | null>('lastfm_import_change_track', { batchId, id, query }),
    changeAlbum: (batchId: number, id: string, query: string) => invoke<void>('lastfm_import_change_album', { batchId, id, query }),
    selectMatch: (batchId: number, id: string, uri: string) => invoke<PageView | null>('lastfm_import_select_match', { batchId, id, uri }),
    selectMatches: (batchId: number, selections: Array<{ id: string; uri: string }>) => invoke<PageView | null>('lastfm_import_select_matches', { batchId, selections }),
    start: (defaults: LastFmImportDefaults) => invoke<LastFmImportState>('start_lastfm_import', { defaults }),
    acceptAll: () => invoke<LastFmImportState>('lastfm_import_accept_all'),
    prepareAcceptAll: () => invoke<{ albumEntities: number; trackEntities: number }>('lastfm_import_prepare_accept_all'),
    setSearchTerms: (show: boolean) => invoke<void>('lastfm_import_search_terms', { show }),
    accountState: () => invoke<LastFmState>('lastfm_state'),
    connectAccount: () => invoke<LastFmState>('connect_lastfm'),
    finishAccount: () => invoke<LastFmState>('finish_lastfm'),
    disconnectAccount: () => invoke<LastFmState>('disconnect_lastfm'),
    syncPlays: () => invoke<LastFmImportState>('sync_lastfm_plays'),
  }
}

export const lastfmGateway = createLastFmGateway(tauriInvoker)
