export type ImportSort = 'plays' | 'artist' | 'batch' | 'lastPlayed'
export type ImportPhase = 'downloading' | 'aggregating' | 'review' | 'done' | 'suspended'
export type CountMode = 'sum' | 'overwrite' | 'zero'
export type ImportPickerKind = 'album' | 'track'
export type ImportConfidence = 'exact' | 'likely' | 'low' | null
export type ImportMatchRelation = 'best-match' | 'same-songs' | 'superset' | null
export type ReviewStatus = 'pending' | 'done' | 'skipped' | 'ignored-album' | 'ignored-artist'
export type QueueStatus = ReviewStatus | 'excluded'

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
}

export type ImportQueuePage = {
  items: ImportQueueItem[]
  cursor: number
  nextCursor: number | null
  total: number
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
  return `track:"${quote(source.track)}" artist:"${quote(source.artist)}"`
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
  return 'low'
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
  const detail = state.phase === null
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
  if (pageLoading) return { title: 'Matching this review batch…', detail: 'Spotify is contacted only for the visible batch.' }
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

export async function loadSelectedImportPage<T>(generation: { current: number }, item: ImportQueueItem, load: (item: ImportQueueItem) => Promise<T>, select: (item: ImportQueueItem) => void, apply: (value: T) => void, invalidate: () => void = () => {}): Promise<void> {
  const requestGeneration = ++generation.current
  invalidate()
  select(item)
  await applyCurrentImportPageResponse(requestGeneration, () => generation.current, load(item), apply)
}
