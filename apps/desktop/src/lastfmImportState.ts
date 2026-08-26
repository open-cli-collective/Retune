export type ImportSort = 'plays' | 'artist' | 'batch' | 'lastPlayed'
export type ImportPhase = 'downloading' | 'aggregating' | 'review' | 'done' | 'suspended'
export type CountMode = 'sum' | 'overwrite' | 'zero'
export type ImportPickerKind = 'album' | 'track'
export type ImportConfidence = 'exact' | 'likely' | 'low' | null
export type ImportMatchRelation = 'best-match' | 'same-songs' | 'superset' | null
export type ImportNavigationTarget = 'queue' | 'source' | 'match'
export type ReviewStatus = 'pending' | 'done' | 'skipped' | 'ignored-album' | 'ignored-artist'
export type QueueStatus = ReviewStatus | 'excluded' | 'failed'

export type ImportShortcutContext = {
  key: string
  navigationTarget: ImportNavigationTarget | null
  control: boolean
  modal: boolean
  altKey: boolean
  ctrlKey: boolean
  metaKey: boolean
  shiftKey: boolean
}

export function canHandleImportShortcut(context: ImportShortcutContext): boolean {
  if (!context.navigationTarget || context.control || context.modal || context.altKey || context.ctrlKey || context.metaKey) return false
  if (!['Tab', 'ArrowUp', 'ArrowDown', 'Enter', 'Escape', ' ', 'e', 'E', 'x', 'X', 's', 'S', 'a', 'A', '?'].includes(context.key)) return false
  return context.key === 'Tab' || context.key === '?' || !context.shiftKey
}

export function moveImportQueueIndex(current: number, itemCount: number, direction: -1 | 1): number {
  return Math.min(Math.max(0, current + direction), Math.max(0, itemCount - 1))
}

export function moveImportNavigationRow(current: number, rowCount: number, direction: -1 | 1): number {
  return Math.min(Math.max(0, current + direction), Math.max(0, rowCount - 1))
}

export function requiredImportMatchIds(selectedIds: Iterable<string>, matchedIds: Iterable<string>, includeHistoricalPlayCounts: boolean, wholeAlbum: boolean): string[] {
  if (!includeHistoricalPlayCounts && wholeAlbum) return []
  const matched = new Set(matchedIds)
  return [...selectedIds].filter((id) => !matched.has(id))
}

export type ImportStrongMatchCandidate = {
  artist: string
  relation: ImportMatchRelation
  trackUris: string[]
}

export type ImportStrongMatchRow = {
  excluded: boolean
  targetUri: string | null
  standaloneLowConfidence?: boolean
}

export type ImportStrongMatch = {
  strong: boolean
  extraTrackCount: number
}

export function normalizeImportMatch(value: string): string {
  return value.toLowerCase().replace(/[^\p{L}\p{N}]/gu, '')
}

export function strongImportAlbumMatch(batchArtist: string, candidate: ImportStrongMatchCandidate, rows: ImportStrongMatchRow[]): ImportStrongMatch {
  const included = rows.filter((row) => !row.excluded)
  const albumUris = new Set(candidate.trackUris)
  const mappedUris = included.map((row) => row.targetUri)
  if (!included.length || (candidate.relation !== 'best-match' && candidate.relation !== 'superset') || normalizeImportMatch(batchArtist) !== normalizeImportMatch(candidate.artist) || included.some((row) => !row.targetUri || !albumUris.has(row.targetUri) || row.standaloneLowConfidence)) return { strong: false, extraTrackCount: 0 }
  const uniqueMappedUris = new Set(mappedUris)
  if (uniqueMappedUris.size !== mappedUris.length) return { strong: false, extraTrackCount: 0 }
  return { strong: true, extraTrackCount: candidate.relation === 'superset' ? Math.max(0, albumUris.size - uniqueMappedUris.size) : 0 }
}

export type ImportVariant = {
  artist: string
  album: string
  track: string
  playCount: number
  earliest: number
  latest: number
}

export type ImportQueueItem = {
  page: number
  artist: string
  album: string
  playCount: number
  latest: number
  sourceCount: number
  remaining: boolean
  albumEntities: number
  trackEntities: number
  status?: QueueStatus | null
  error?: string | null
  retryAt?: number | null
}

export function spotifyLimitCountdown(retryAt: number, now: number): string {
  const remaining = Math.max(0, Math.ceil(retryAt - now))
  const hours = Math.floor(remaining / 3600)
  const minutes = Math.floor((remaining % 3600) / 60)
  const seconds = remaining % 60
  return [hours && `${hours}h`, (hours || minutes) && `${minutes}m`, `${seconds}s`].filter(Boolean).join(' ')
}

export type ImportQueuePage = {
  items: ImportQueueItem[]
  cursor: number
  nextCursor: number | null
  total: number
}

export function shouldRefreshImportEvent(acceptAllRunning: boolean, queueMutationRunning: boolean): boolean {
  return !acceptAllRunning && !queueMutationRunning
}

export function importQueueHighlightIndex(items: Pick<ImportQueueItem, 'page'>[], highlightedPage: number | null, selectedPage: number | null, currentIndex: number): number {
  const highlightedIndex = highlightedPage === null ? -1 : items.findIndex((item) => item.page === highlightedPage)
  if (highlightedIndex >= 0) return highlightedIndex
  const selectedIndex = selectedPage === null ? -1 : items.findIndex((item) => item.page === selectedPage)
  return selectedIndex >= 0 ? selectedIndex : Math.min(Math.max(currentIndex, 0), Math.max(0, items.length - 1))
}

export function activeImportQueue(items: ImportQueueItem[]): ImportQueueItem[] {
  return items.filter((item) => item.remaining || item.status === 'failed')
}

export function importQueueTabTarget(shiftKey: boolean): { kind: 'source' | 'match'; row: 0 } {
  return { kind: shiftKey ? 'match' : 'source', row: 0 }
}

export type ImportQueueTabTarget = Pick<HTMLElement, 'focus' | 'scrollIntoView'>

export function handleImportQueueTab(event: Pick<KeyboardEvent, 'preventDefault' | 'stopPropagation'>, target: ImportQueueTabTarget | null): boolean {
  if (!target) return false
  event.preventDefault()
  event.stopPropagation()
  target.focus()
  target.scrollIntoView({ block: 'nearest' })
  return true
}

export function importAlbumActionAdvances(action: 'skip-album' | 'restore'): boolean {
  return action === 'skip-album'
}

export type ImportSourceRow = {
  stableId: string
  artist: string
  album: string
  track: string
  playCount: number
  earliest: number
  latest: number
  variants: ImportVariant[]
}

export type ImportCollectionSuggestionCandidate = {
  uri: string
  artist: string
  inLibrary: boolean
  relation: ImportMatchRelation
}

export type ImportCollectionSuggestionMatch = {
  selectedUri: string | null
  trackMatches: Record<string, string>
  candidates: ImportCollectionSuggestionCandidate[]
}

export type CollectionAmbiguousChoice = {
  uri: string
  album: string
  projectedMatches: number
  totalTracks: number
  recommended: boolean
}

export function collectionAmbiguousChoices(
  sourceId: string,
  match: ImportCollectionSuggestionMatch | null,
  albums: Array<{ uri: string; name: string; trackUris: string[] }>,
  selectedAlbumUris: string[],
  coverage: Array<{ uri: string; matched: number; uniqueCoverage: number }>,
): CollectionAmbiguousChoice[] {
  if (!match || match.selectedUri || match.trackMatches[sourceId]) return []
  const selected = new Set(selectedAlbumUris)
  const supportedUris = new Set(match.candidates.filter((candidate) => candidate.uri.startsWith('spotify:track:') && (candidate.relation === 'best-match' || candidate.relation === 'same-songs')).map((candidate) => candidate.uri))
  const choices = albums.flatMap((album, order) => {
    if (!selected.has(album.uri)) return []
    const albumCoverage = coverage.find((entry) => entry.uri === album.uri)
    return album.trackUris.filter((uri) => supportedUris.has(uri)).map((uri) => ({
      uri,
      album: album.name,
      projectedMatches: Math.min(album.trackUris.length, (albumCoverage?.matched ?? 0) + 1),
      totalTracks: album.trackUris.length,
      uniqueCoverage: albumCoverage?.uniqueCoverage ?? 0,
      order,
    }))
  })
  if (choices.length < 2) return []
  choices.sort((left, right) => right.projectedMatches - left.projectedMatches || right.uniqueCoverage - left.uniqueCoverage || left.order - right.order)
  const recommendationIsDistinct = choices[0].projectedMatches !== choices[1].projectedMatches || choices[0].uniqueCoverage !== choices[1].uniqueCoverage
  return choices.map(({ uniqueCoverage: _uniqueCoverage, order: _order, ...choice }, index) => ({ ...choice, recommended: index === 0 && recommendationIsDistinct }))
}

export function collectionSuggestion<T extends ImportCollectionSuggestionCandidate>(source: Pick<ImportSourceRow, 'stableId' | 'artist' | 'variants'>, match: (Omit<ImportCollectionSuggestionMatch, 'candidates'> & { candidates: T[] }) | null, selectedTrackUris: Iterable<string> = []): T | null {
  if (!match || match.selectedUri || match.trackMatches[source.stableId]) return null
  const artists = [source.artist, ...source.variants.map((variant) => variant.artist)].map(normalizeImportMatch)
  const selected = new Set(selectedTrackUris)
  const candidates = new Map(match.candidates
    .filter((candidate) => candidate.uri.startsWith('spotify:track:') && candidate.relation === 'same-songs' && (candidate.inLibrary || selected.has(candidate.uri)) && artists.includes(normalizeImportMatch(candidate.artist)))
    .map((candidate) => [candidate.uri, candidate]))
  return candidates.size === 1 ? candidates.values().next().value ?? null : null
}

export function stablePartitionImportRows<T>(rows: T[], requiredIds: Iterable<string>, id: (row: T) => string): T[] {
  const required = new Set(requiredIds)
  return [...rows.filter((row) => required.has(id(row))), ...rows.filter((row) => !required.has(id(row)))]
}

export type CollectionAlbumProjection = { uri: string }

export type CollectionTrackMatchStatus = 'matched' | 'ambiguous' | 'unmatched'

export type CollectionTrackStatusProjection = { uri: string; status: CollectionTrackMatchStatus }

export function collectionImportBranch(album: string): 'collection' | 'release' {
  return album === '' ? 'collection' : 'release'
}

export function selectedCollectionAlbumUris(cached: CollectionAlbumProjection[], selectedUris: Iterable<string>): string[] {
  const cachedUris = new Set(cached.map((candidate) => candidate.uri))
  const seen = new Set<string>()
  const result: string[] = []
  for (const uri of selectedUris) {
    if (cachedUris.has(uri) && !seen.has(uri)) {
      seen.add(uri)
      result.push(uri)
    }
  }
  return result
}

export function collectionDialogScreen(previewUri: string | undefined, cached: CollectionAlbumProjection[]): 'results' | 'preview' {
  return previewUri && cached.some((candidate) => candidate.uri === previewUri) ? 'preview' : 'results'
}

export type CollectionDialogState<T extends CollectionAlbumProjection = CollectionAlbumProjection> = {
  query: string
  results: T[]
  previewUri?: string
  resultsScrollTop: number
  pendingPreview: { previousPreviewUri?: string; previousResultsScrollTop: number } | null
}

export type CollectionDialogAction<T extends CollectionAlbumProjection = CollectionAlbumProjection> =
  | { type: 'set-query'; query: string }
  | { type: 'search-succeeded'; results: T[] }
  | { type: 'search-failed' }
  | { type: 'preview-started'; uri: string; resultsScrollTop: number }
  | { type: 'preview-succeeded'; uri: string }
  | { type: 'preview-add-succeeded' }
  | { type: 'preview-failed' }
  | { type: 'back-to-results' }

export function collectionDialogInitialState<T extends CollectionAlbumProjection = CollectionAlbumProjection>(previewUri?: string): CollectionDialogState<T> {
  return { query: '', results: [], previewUri, resultsScrollTop: 0, pendingPreview: null }
}

export function collectionDialogTransition<T extends CollectionAlbumProjection>(state: CollectionDialogState<T>, action: CollectionDialogAction<T>): CollectionDialogState<T> {
  switch (action.type) {
    case 'set-query': return { ...state, query: action.query }
    case 'search-succeeded': return { ...state, results: action.results }
    case 'search-failed': return state
    case 'preview-started': return {
      ...state,
      previewUri: action.uri,
      resultsScrollTop: action.resultsScrollTop,
      pendingPreview: { previousPreviewUri: state.previewUri, previousResultsScrollTop: action.resultsScrollTop },
    }
    case 'preview-succeeded': return { ...state, previewUri: action.uri, pendingPreview: null }
    case 'preview-add-succeeded': return { ...state, previewUri: undefined, pendingPreview: null }
    case 'preview-failed': return state.pendingPreview ? {
      ...state,
      previewUri: state.pendingPreview.previousPreviewUri,
      resultsScrollTop: state.pendingPreview.previousResultsScrollTop,
      pendingPreview: null,
    } : state
    case 'back-to-results': return { ...state, previewUri: undefined, pendingPreview: null }
  }
}

export function collectionAlbumActionLabel(selected: boolean): 'Add to album matches' | 'Remove from album matches' {
  return selected ? 'Remove from album matches' : 'Add to album matches'
}

export function collectionCoverageStatus(coverage: { matched: number; ambiguous: number; unresolved: number }): string {
  return `${coverage.matched} matched · ${coverage.ambiguous} ambiguous · ${coverage.unresolved} unresolved`
}

export function collectionAlbumTrackStatuses(trackUris: string[], statuses: CollectionTrackStatusProjection[]): CollectionTrackMatchStatus[] {
  const byUri = new Map(statuses.map((track) => [track.uri, track.status]))
  return trackUris.map((uri) => byUri.get(uri) ?? 'unmatched')
}

export function collectionPreviewCoverageCopy(coverage: { selected: boolean; matched: number; uniqueCoverage: number; marginalMatches: number; ambiguityChanges: number }): string {
  if (coverage.selected) return `${coverage.matched} matches · ${coverage.uniqueCoverage} unique`
  const signed = (value: number) => `${value >= 0 ? '+' : ''}${value}`
  return `${signed(coverage.marginalMatches)} marginal matches · ${signed(coverage.ambiguityChanges)} ambiguity change`
}

export type ImportDecision = { status: ReviewStatus; excluded: boolean }

export type ReviewState = {
  rows: ImportSourceRow[]
  decisions: Record<string, ImportDecision>
  checked: Set<string>
  importContent: boolean
  includeHistoricalPlayCounts: boolean
  wholeAlbum: boolean
  genre: string
  rating: number | null
}

const pending: ImportDecision = { status: 'pending', excluded: false }

export function defaultReviewState(rows: ImportSourceRow[]): ReviewState {
  return {
    rows,
    decisions: Object.fromEntries(rows.map((row) => [row.stableId, { ...pending }])),
    checked: new Set(rows.map((row) => row.stableId)),
    importContent: true,
    includeHistoricalPlayCounts: true,
    wholeAlbum: false,
    genre: '',
    rating: null,
  }
}

function withDecision(state: ReviewState, ids: string[], decision: Partial<ImportDecision>): ReviewState {
  const decisions = { ...state.decisions }
  for (const id of ids) decisions[id] = { ...(decisions[id] ?? pending), ...decision }
  return { ...state, decisions }
}

function reviewable(state: ReviewState, id: string) {
  const decision = state.decisions[id] ?? pending
  return decision.status === 'pending' || decision.status === 'skipped'
}

function albumIds(state: ReviewState, artist: string, album: string) {
  return state.rows.filter((row) => row.artist === artist && row.album === album && reviewable(state, row.stableId) && !state.decisions[row.stableId]?.excluded).map((row) => row.stableId)
}

export function toggleImportRow(state: ReviewState, id: string): ReviewState {
  const decision = state.decisions[id] ?? pending
  if (decision.excluded || !reviewable(state, id)) return state
  const checked = new Set(state.checked)
  if (!checked.delete(id)) checked.add(id)
  return { ...state, checked }
}

export function excludeImportRow(state: ReviewState, id: string, excluded = true): ReviewState {
  if (!reviewable(state, id)) return state
  return withDecision(state, [id], { excluded })
}

export function skipImportAlbum(state: ReviewState, artist: string, album: string): ReviewState {
  return withDecision(state, albumIds(state, artist, album), { status: 'skipped' })
}

export function ignoreImportAlbum(state: ReviewState, artist: string, album: string): ReviewState {
  return withDecision(state, albumIds(state, artist, album), { status: 'ignored-album' })
}

export function ignoreImportArtist(state: ReviewState, artist: string): ReviewState {
  return withDecision(state, state.rows.filter((row) => row.artist === artist && reviewable(state, row.stableId) && !state.decisions[row.stableId]?.excluded).map((row) => row.stableId), { status: 'ignored-artist' })
}

export function restoreImportAlbum(state: ReviewState, artist: string, album: string): ReviewState {
  return withDecision(state, albumIds(state, artist, album), pending)
}

export function acceptImportChanges(state: ReviewState): { state: ReviewState; committed: string[] } {
  const committed = [...state.checked].filter((id) => {
    const decision = state.decisions[id] ?? pending
    return !decision.excluded && (decision.status === 'pending' || decision.status === 'skipped')
  })
  return { state: withDecision(state, committed, { status: 'done' }), committed }
}

export function acceptImportAndNext(state: ReviewState): { state: ReviewState; committed: string[]; advance: true } {
  const accepted = acceptImportChanges(state)
  return { ...accepted, advance: true }
}

export function remainingImportCount(state: ReviewState): number {
  return state.rows.filter((row) => {
    const decision = state.decisions[row.stableId] ?? pending
    return !decision.excluded && (decision.status === 'pending' || decision.status === 'skipped')
  }).length
}

export function selectedImportCount(state: ReviewState): number {
  return [...state.checked].filter((id) => {
    const decision = state.decisions[id] ?? pending
    return !decision.excluded && (decision.status === 'pending' || decision.status === 'skipped')
  }).length
}

export function excludedImportCount(state: ReviewState): number {
  return state.rows.filter((row) => state.decisions[row.stableId]?.excluded).length
}

export function restPendingImportCount(state: ReviewState): number {
  return state.rows.filter((row) => {
    const decision = state.decisions[row.stableId] ?? pending
    return !decision.excluded && (decision.status === 'pending' || decision.status === 'skipped') && !state.checked.has(row.stableId)
  }).length
}

export function validImportIntent(importContent: boolean, includeHistoricalPlayCounts: boolean): boolean {
  return importContent || includeHistoricalPlayCounts
}

export function trackPickerQuery(source: Pick<ImportSourceRow, 'artist' | 'track'>): string {
  const quote = (value: string) => value.replace(/"/g, ' ')
  const simplified = source.track.replace(/\([^)]*\)/g, ' ').replace(/\//g, ' ').replace(/\s+/g, ' ').trim()
  return `track:"${quote(simplified || source.track)}" artist:"${quote(source.artist)}"`
}

export function pickerCandidates<T extends { uri: string }>(kind: ImportPickerKind, candidates: T[]): T[] {
  const prefix = `spotify:${kind}:`
  return candidates.filter((candidate) => candidate.uri.startsWith(prefix))
}

export function pickerSelectedUri(kind: ImportPickerKind, sourceId: string, selectedUri: string | null, trackMatches: Record<string, string>): string | null {
  if (kind === 'track') return trackMatches[sourceId] ?? (selectedUri?.startsWith('spotify:track:') ? selectedUri : null)
  return selectedUri?.startsWith('spotify:album:') ? selectedUri : null
}

export function selectedImportTrackConfidence(sourceId: string, selectedUri: string | null, trackMatches: Record<string, string>, albumConfidence: ImportConfidence, candidates: Array<{ uri: string; relation: ImportMatchRelation }>): ImportConfidence {
  const targetUri = trackMatches[sourceId] ?? (selectedUri?.startsWith('spotify:track:') ? selectedUri : null)
  const standalone = targetUri?.startsWith('spotify:track:') ? candidates.find((candidate) => candidate.uri === targetUri) : undefined
  if (!standalone) return albumConfidence
  if (standalone.relation === 'best-match') return 'exact'
  if (standalone.relation === 'same-songs' || standalone.relation === 'superset') return 'likely'
  return selectedUri?.startsWith('spotify:album:') ? albumConfidence : 'low'
}

export function resolveImportCount(rows: ImportSourceRow[], mode: CountMode): number {
  if (mode === 'zero') return 0
  if (mode === 'overwrite') return Math.max(0, ...rows.flatMap((row) => row.variants.map((variant) => variant.playCount)))
  return rows.reduce((total, row) => total + row.playCount, 0)
}

export function sortImportQueue(items: ImportQueueItem[], sort: ImportSort): ImportQueueItem[] {
  return [...items].sort((left, right) => {
    if (sort === 'artist') return left.artist.localeCompare(right.artist) || left.album.localeCompare(right.album) || left.page - right.page
    if (sort === 'batch') return right.sourceCount - left.sourceCount || left.artist.localeCompare(right.artist) || left.page - right.page
    if (sort === 'lastPlayed') return right.latest - left.latest || left.artist.localeCompare(right.artist) || left.page - right.page
    return right.playCount - left.playCount || left.artist.localeCompare(right.artist) || left.page - right.page
  })
}

export function nextRemainingImportQueue(items: ImportQueueItem[], current: ImportQueueItem | null, sort: ImportSort): ImportQueueItem | null {
  const ordered = sortImportQueue(items, sort)
  const currentIndex = current ? ordered.findIndex((item) => item.page === current.page) : -1
  return ordered.slice(currentIndex + 1).find((item) => item.remaining) ?? ordered.slice(0, Math.max(0, currentIndex)).find((item) => item.remaining) ?? null
}

export function projectAcknowledgedImportApply(items: ImportQueueItem[], appliedPage: number, sort: ImportSort): { queue: ImportQueueItem[]; next: ImportQueueItem | null } {
  const current = items.find((item) => item.page === appliedPage) ?? null
  const queue = items.map((item) => item.page === appliedPage ? { ...item, remaining: false, status: 'done' as const, error: null } : item)
  return { queue, next: nextRemainingImportQueue(queue, current, sort) }
}

export type ImportQueueVisibleRange = { start: number; end: number; offsetTop: number; contentHeight: number }

export function importQueueVisibleRange(itemCount: number, scrollTop: number, viewportHeight: number, rowHeight = 57, overscan = 4): ImportQueueVisibleRange {
  const count = Math.max(0, Math.floor(Number.isFinite(itemCount) ? itemCount : 0))
  const height = Math.max(1, rowHeight)
  const viewport = Math.max(0, Number.isFinite(viewportHeight) ? viewportHeight : 0)
  const contentHeight = count * height
  const top = Math.min(Math.max(0, Number.isFinite(scrollTop) ? scrollTop : 0), Math.max(0, contentHeight - viewport))
  const first = Math.floor(top / height)
  const last = Math.min(count, Math.max(first + 1, Math.ceil((top + viewport) / height)))
  const extra = Math.max(0, Math.floor(overscan))
  const start = Math.max(0, first - extra)
  const end = Math.min(count, last + extra)
  return { start, end, offsetTop: start * height, contentHeight }
}

export type ImportDownloadState = {
  phase: ImportPhase | null
  processedScrobbles: number
  totalScrobbles: number
  downloadedPages: number
  totalPages: number | null
  downloadedThrough: number | null
  historyTo: number | null
  syncing?: boolean
  pendingReview?: number
  syncProblem?: string | null
}

export function importDownloadPercent(downloadedPages: number, totalPages: number | null): number {
  return totalPages ? Math.min(100, Math.round((downloadedPages / totalPages) * 100)) : 0
}

export function importDownloadProgressLabel(processedScrobbles: number, totalScrobbles: number, percent: number): string {
  const total = Math.max(0, totalScrobbles)
  const processed = Math.min(total, Math.max(0, processedScrobbles))
  return `${processed.toLocaleString()} of ${total.toLocaleString()} plays downloaded · ${percent}%`
}

export function formatImportDate(timestamp: number | null, locale?: string): string | null {
  if (!timestamp || !Number.isFinite(timestamp)) return null
  const date = new Date(timestamp * 1000)
  if (Number.isNaN(date.getTime())) return null
  return new Intl.DateTimeFormat(locale, { month: 'long', year: 'numeric', timeZone: 'UTC' }).format(date)
}

export function importHistoryBreadcrumb(downloadedThrough: number | null, historyTo: number | null): string {
  const reached = formatImportDate(downloadedThrough)
  const historyEnd = formatImportDate(historyTo)
  const end = historyEnd ? ` · history ends ${historyEnd}` : ''
  return reached ? `Oldest → newest · reached ${reached}${end}` : `Oldest → newest · starting with the oldest plays${end}`
}

export function importDownloadCopy(state: ImportDownloadState): { detail: string; progress: string; breadcrumb: string } {
  const detail = state.syncing
    ? 'Retune is downloading only the new Last.fm plays. Retune-origin scrobbles are deduplicated locally, and the visible review queue stays available while this runs.'
    : state.syncProblem
      ? state.syncProblem
      : state.phase === null
    ? 'Retune takes a fixed snapshot of your Last.fm plays and moves from oldest to newest. You can review every match before anything is applied.'
    : state.phase === 'suspended'
      ? 'Reconnect the saved Last.fm account before resuming this session.'
      : state.phase === 'aggregating'
        ? 'All plays are downloaded. Retune is sorting and grouping them before review.'
        : 'Retune downloads your oldest plays first and moves toward your newest plays.'
  return {
    detail,
    progress: importDownloadProgressLabel(state.processedScrobbles, state.totalScrobbles, importDownloadPercent(state.downloadedPages, state.totalPages)),
    breadcrumb: importHistoryBreadcrumb(state.downloadedThrough, state.historyTo),
  }
}

export function importStatusLabel(status: QueueStatus): string {
  return status === 'ignored-album' ? 'ignored-album' : status === 'ignored-artist' ? 'ignored-artist' : status
}

export function importStatusText(phase: ImportPhase | null, username: string | null): string {
  if (phase === 'downloading') return 'Downloading Last.fm plays'
  if (phase === 'aggregating') return 'Preparing Last.fm review'
  if (phase === 'suspended') return 'Import suspended for account safety'
  if (phase === 'done') return 'Import complete'
  return username ? 'Ready to import' : 'Connect Last.fm to begin'
}

export function showsImportRemaining(phase: ImportPhase | null): boolean {
  return phase === 'review' || phase === 'done'
}

export function downloadAction(phase: ImportPhase | null, retryableError: { retryable: boolean } | null): { label: string; disabled: boolean } {
  if (phase === null) return { label: 'Start import', disabled: false }
  if (phase === 'suspended') return { label: 'Check accounts and resume', disabled: false }
  if (retryableError?.retryable && (phase === 'downloading' || phase === 'aggregating')) return { label: 'Retrying automatically…', disabled: true }
  if (phase === 'aggregating' && !retryableError) return { label: 'Preparing review…', disabled: true }
  if (phase === 'downloading' && !retryableError) return { label: 'Downloading…', disabled: true }
  return { label: 'Resume download', disabled: false }
}

export function importEmptyPageMessage(phase: ImportPhase | null, pageLoading: boolean): { title: string; detail: string } {
  if (pageLoading) return { title: 'Matching this review batch…', detail: 'Searching Spotify for likely matches.' }
  if (phase === 'done') return { title: 'Import complete', detail: 'All review batches are complete.' }
  return { title: 'No review page selected', detail: 'Select an album from the queue.' }
}

export function isCurrentImportPageResponse(requestGeneration: number, currentGeneration: number): boolean {
  return requestGeneration === currentGeneration
}

export async function applyCurrentImportPageResponse<T>(requestGeneration: number, currentGeneration: () => number, response: Promise<T>, apply: (value: T) => void): Promise<void> {
  const value = await response
  if (isCurrentImportPageResponse(requestGeneration, currentGeneration())) apply(value)
}

export async function loadSelectedImportPage<T>(generation: { current: number }, item: ImportQueueItem, load: (item: ImportQueueItem) => Promise<T>, select: (item: ImportQueueItem) => void, apply: (value: T) => void, invalidate: () => void = () => {}, complete: () => void = () => {}): Promise<void> {
  const requestGeneration = ++generation.current
  try {
    invalidate()
    select(item)
    await applyCurrentImportPageResponse(requestGeneration, () => generation.current, load(item), apply)
  } finally {
    if (isCurrentImportPageResponse(requestGeneration, generation.current)) complete()
  }
}
