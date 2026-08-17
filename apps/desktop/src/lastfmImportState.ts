export type ImportSort = 'plays' | 'artist' | 'batch' | 'lastPlayed'
export type CountMode = 'sum' | 'overwrite' | 'zero'
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
  artist: string
  album: string
  playCount: number
  latest: number
  sourceIds: string[]
  remaining: boolean
  albumEntities: number
  trackEntities: number
  status?: QueueStatus | null
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

export function resolveImportCount(rows: ImportSourceRow[], mode: CountMode): number {
  if (mode === 'zero') return 0
  if (mode === 'overwrite') return Math.max(0, ...rows.flatMap((row) => row.variants.map((variant) => variant.playCount)))
  return rows.reduce((total, row) => total + row.playCount, 0)
}

export function sortImportQueue(items: ImportQueueItem[], sort: ImportSort): ImportQueueItem[] {
  return [...items].sort((left, right) => {
    if (sort === 'artist') return left.artist.localeCompare(right.artist) || left.album.localeCompare(right.album)
    if (sort === 'batch') return right.sourceIds.length - left.sourceIds.length || left.artist.localeCompare(right.artist)
    if (sort === 'lastPlayed') return right.latest - left.latest || left.artist.localeCompare(right.artist)
    return right.playCount - left.playCount || left.artist.localeCompare(right.artist)
  })
}

export function nextRemainingImportQueue(items: ImportQueueItem[], current: ImportQueueItem | null, sort: ImportSort): ImportQueueItem | null {
  const ordered = sortImportQueue(items, sort)
  const currentIndex = current ? ordered.findIndex((item) => item.artist === current.artist && item.album === current.album) : -1
  return ordered.slice(currentIndex + 1).find((item) => item.remaining) ?? ordered.slice(0, Math.max(0, currentIndex)).find((item) => item.remaining) ?? null
}

export function importStatusLabel(status: QueueStatus): string {
  return status === 'ignored-album' ? 'ignored-album' : status === 'ignored-artist' ? 'ignored-artist' : status
}

export function isCurrentImportPageResponse(requestGeneration: number, currentGeneration: number): boolean {
  return requestGeneration === currentGeneration
}
