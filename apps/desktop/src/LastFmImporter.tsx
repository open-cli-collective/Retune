import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent, type PointerEvent as ReactPointerEvent, type RefObject } from 'react'
import { SpotifyAlbumPresentation, type SpotifyAlbumPresentationData } from './spotifyViews.tsx'
import { ModalDialog } from './viewShared.tsx'
import type { LastFmImportDefaults, LastFmImportState, Settings } from './types.ts'
import { activeImportQueue, applyCurrentImportPageResponse, automaticCollectionAlbumContributors, canHandleImportShortcut, collectionAlbumActionLabel, collectionAlbumTrackStatuses, collectionAmbiguousChoices, collectionCoverageStatus, collectionDialogInitialState, collectionDialogScreen, collectionDialogTransition, collectionPreviewCoverageCopy, collectionSuggestion, downloadAction, excludedImportCount, handleImportQueueTab, importAlbumActionAdvances, importCountMergePresentation, importDownloadCopy, importDownloadPercent, importEmptyPageMessage, importQueueHighlightIndex, importQueueTabTarget, importQueueVisibleRange, importStatusText, isCurrentImportPageResponse, loadSelectedImportPage, moveImportNavigationRow, moveImportQueueIndex, nextRemainingImportQueue, pickerCandidates, pickerSelectedUri, projectAcknowledgedImportApply, requiredImportMatchIds, restPendingImportCount, selectedCollectionAlbumUris, selectedImportCount, selectedImportTrackConfidence, shouldRefreshImportEvent, showsImportRemaining, sortImportQueue, spotifyLimitCountdown, stablePartitionImportRows, strongImportAlbumMatch, toggleImportRow, trackPickerQuery, validImportIntent, type CollectionAmbiguousChoice, type CollectionTrackStatusProjection, type CountMode, type ImportConfidence, type ImportNavigationTarget, type ImportPickerKind, type ImportQueueItem, type ImportQueuePage, type ImportQueueTabTarget, type ImportSourceRow, type ReviewState } from './lastfmImportState.ts'
import './lastfmImporter.css'

type ImportStateView = LastFmImportState
type AlbumCandidate = { uri: string; name: string; artist: string; inLibrary: boolean; relation: 'best-match' | 'same-songs' | 'superset' | null; trackUris: string[]; trackNames: string[]; trackArtists: string[]; trackAlbums: string[]; imageUrl?: string | null; releaseDate?: string | null; albumType?: string | null; totalTracks?: number; trackNumbers?: (number | null)[]; trackDurations?: number[] }
type CollectionAlbumCoverage = { uri: string; matched: number; uniqueCoverage: number }
type CollectionAlbumPreviewCoverage = { uri: string; selected: boolean; matched: number; uniqueCoverage: number; marginalMatches: number; ambiguityChanges: number; trackStatuses: CollectionTrackStatusProjection[] }
type CollectionMatchView = { cachedAlbums: AlbumCandidate[]; selectedAlbumUris: string[]; wholeAlbumReady: boolean; coverage: { matched: number; ambiguous: number; unresolved: number; selectedAlbums: CollectionAlbumCoverage[]; previews: CollectionAlbumPreviewCoverage[] } }
type MatchResult = { sourceId: string; searchTerm: string; confidence: 'exact' | 'likely' | 'low' | null; selectedUri: string | null; candidates: AlbumCandidate[]; trackMatches: Record<string, string> }
type PageItem = { source: ImportSourceRow; decision: { status: 'pending' | 'done' | 'skipped' | 'ignored-album' | 'ignored-artist'; excluded: boolean }; matchResult: MatchResult | null }
type PageView = { state: ImportStateView; batchId: number; artist: string; album: string; pageNumber: number; pageCount: number; rows: PageItem[]; options: { importContent: boolean; includeHistoricalPlayCounts: boolean; wholeAlbum: boolean; genre: string | null; rating: number | null; selectedTrackIds: string[] }; fuzzyGroups: Record<string, ImportSourceRow[]>; countModes: Record<string, CountMode>; resolvedCounts: Record<string, number>; lockedCountModes: string[]; collection: CollectionMatchView | null }
type PickerKind = ImportPickerKind
type PickerState = { kind: PickerKind; sourceId: string; query: string }
type FuzzyProps = { fuzzy?: ImportSourceRow[]; fuzzyTarget?: string; fuzzyResultCount: number; fuzzyExpanded: boolean; fuzzyMode: CountMode; fuzzyLocked: boolean; onFuzzyMode: (mode: CountMode) => void; onFuzzyToggle: () => void }
type ShortcutStatus = (message: string) => void

const IMPORT_NAV_KEYS = 'ArrowUp ArrowDown Tab Shift+Tab Enter E Space X S A Escape ?'

const emptyDefaults: LastFmImportDefaults = { importContent: true, includeHistoricalPlayCounts: true, wholeAlbum: false }
const emptyState: ImportStateView = { phase: null, username: null, spotifyAccountId: null, historyTo: null, downloadedThrough: null, nextPage: 1, totalPages: null, downloadedPages: 0, totalScrobbles: 0, includedScrobbles: 0, processedScrobbles: 0, defaults: emptyDefaults, remaining: 0, retryableError: null, searchTerms: true, syncing: false, lastSyncedAt: null, pendingReview: 0, syncProblem: null, applyingAll: false, spotifyLimit: null }
const importQueuePageLimit = 1000

function SpotifyLimitNotice({ message, retryAt }: { message: string; retryAt?: number | null }) {
  const limited = message.startsWith('Spotify rate limited') || message.startsWith('Spotify Development Mode quota exhausted')
  const [now, setNow] = useState(() => Date.now() / 1000)
  useEffect(() => {
    if (!limited || !retryAt) return
    const timer = window.setInterval(() => {
      const next = Date.now() / 1000
      setNow(next)
      if (next >= retryAt) window.clearInterval(timer)
    }, 1000)
    return () => window.clearInterval(timer)
  }, [limited, retryAt])
  if (!limited) return null
  if (!retryAt) return <span className="import-limit-reset">Spotify did not provide a reset time for this limit.</span>
  const date = new Date(retryAt * 1000)
  return <span className="import-limit-reset">{now >= retryAt ? 'Spotify’s reported wait has ended; retry now.' : <>Available again <time dateTime={date.toISOString()}>{date.toLocaleString()}</time> · {spotifyLimitCountdown(retryAt, now)} remaining</>}</span>
}

async function loadImportQueue(): Promise<ImportQueueItem[]> {
  const items: ImportQueueItem[] = []
  let cursor = 0
  let total: number | undefined
  while (true) {
    const page = await invoke<ImportQueuePage>('lastfm_import_queue', { cursor, limit: importQueuePageLimit })
    if (page.cursor !== cursor || page.items.length > importQueuePageLimit || (total !== undefined && page.total !== total)) throw new Error('Last.fm import queue pagination is inconsistent.')
    total ??= page.total
    items.push(...page.items)
    if (page.nextCursor === null) {
      if (items.length !== page.total) throw new Error('Last.fm import queue pagination is incomplete.')
      return items
    }
    if (!Number.isSafeInteger(page.nextCursor) || page.nextCursor <= cursor || page.nextCursor > page.total) throw new Error('Last.fm import queue pagination is invalid.')
    cursor = page.nextCursor
  }
}

function reviewForPage(page: PageView): ReviewState {
  const rows = page.rows.map((item) => item.source)
  const decisions = Object.fromEntries(page.rows.map((item) => [item.source.stableId, item.decision]))
  return {
    rows,
    decisions,
    checked: new Set(page.options.selectedTrackIds),
    importContent: page.options.importContent,
    includeHistoricalPlayCounts: page.options.includeHistoricalPlayCounts,
    wholeAlbum: page.options.wholeAlbum,
    genre: page.options.genre ?? '',
    rating: page.options.rating,
  }
}

function pageOptions(review: ReviewState) {
  return {
    importContent: review.importContent,
    includeHistoricalPlayCounts: review.includeHistoricalPlayCounts,
    wholeAlbum: review.wholeAlbum,
    genre: review.genre || null,
    rating: review.rating,
    selectedTrackIds: [...review.checked],
  }
}

function pageWithQueuePosition(page: PageView | null, orderedQueue: ImportQueueItem[]): PageView | null {
  if (!page) return null
  const index = orderedQueue.findIndex((item) => item.page === page.batchId)
  return { ...page, pageNumber: index + 1, pageCount: orderedQueue.length }
}

function matchedTrack(item: PageItem) {
  const match = item.matchResult
  if (!match) return null
  const targetUri = match.trackMatches[item.source.stableId] ?? (match.selectedUri?.startsWith('spotify:track:') ? match.selectedUri : null)
  if (!targetUri) return null
  const candidate = match.candidates.find((entry) => entry.trackUris.includes(targetUri))
  if (!candidate) return null
  const index = candidate.trackUris.indexOf(targetUri)
  return {
    uri: targetUri,
    name: candidate.trackNames[index] || candidate.name,
    artist: candidate.trackArtists[index] || candidate.artist,
    album: candidate.trackAlbums[index] || candidate.name,
    inLibrary: candidate.inLibrary,
  }
}

function collectionSummary(page: PageView, selectedTrackUris: Iterable<string> = []) {
  let automatic = 0
  let suggested = 0
  let needsReview = 0
  for (const item of page.rows) {
    const track = matchedTrack(item)
    if (track && !item.matchResult?.selectedUri && item.matchResult?.confidence === 'exact') automatic += 1
    else if (collectionSuggestion(item.source, item.matchResult, selectedTrackUris)) suggested += 1
    else if (!track) needsReview += 1
  }
  return { automatic, suggested, needsReview }
}

function confidenceLabel(confidence: MatchResult['confidence']) {
  return confidence === 'exact' ? 'EXACT TRACK MATCH' : confidence === 'likely' ? 'Likely' : confidence === 'low' ? 'Low' : 'Unmatched'
}

function relationLabel(relation: AlbumCandidate['relation']) {
  return relation === 'best-match' ? 'Best match' : relation === 'same-songs' ? 'Same songs' : relation === 'superset' ? 'Superset' : 'Unclassified'
}

function collectionAlbumPresentation(candidate: AlbumCandidate, trackStatuses: CollectionTrackStatusProjection[] = []): SpotifyAlbumPresentationData {
  const statuses = collectionAlbumTrackStatuses(candidate.trackUris, trackStatuses)
  return {
    uri: candidate.uri,
    name: candidate.name,
    artist: candidate.artist,
    albumType: candidate.albumType,
    year: candidate.releaseDate?.slice(0, 4) ?? null,
    imageUrl: candidate.imageUrl,
    tracks: candidate.trackUris.map((uri, index) => ({
      uri,
      name: candidate.trackNames[index] ?? candidate.name,
      trackNo: candidate.trackNumbers?.[index] ?? index + 1,
      durationSecs: candidate.trackDurations?.[index] ?? 0,
      matchState: statuses[index],
    })),
  }
}

function importNavigationTarget(element: EventTarget | null): HTMLElement | null {
  return element instanceof Element ? element.closest<HTMLElement>('[data-import-nav]') : null
}

function VerticalResizeHandle<T extends HTMLElement>({ target, label, minHeight }: { target: RefObject<T | null>; label: string; minHeight: number }) {
  const resize = (clientY: number) => {
    const element = target.current
    if (!element) return
    const height = Math.min(window.innerHeight * .55, Math.max(minHeight, clientY - element.getBoundingClientRect().top))
    element.style.height = `${height}px`
  }
  return <div className="import-resize-handle" role="separator" aria-label={label} aria-orientation="horizontal" tabIndex={0} onPointerDown={(event) => { event.currentTarget.setPointerCapture(event.pointerId); resize(event.clientY) }} onPointerMove={(event) => { if (event.currentTarget.hasPointerCapture(event.pointerId)) resize(event.clientY) }} onPointerUp={(event) => event.currentTarget.releasePointerCapture(event.pointerId)} onPointerCancel={(event) => { if (event.currentTarget.hasPointerCapture(event.pointerId)) event.currentTarget.releasePointerCapture(event.pointerId) }} onKeyDown={(event) => { if (event.key !== 'ArrowUp' && event.key !== 'ArrowDown') return; event.preventDefault(); const element = target.current; if (element) resize(element.getBoundingClientRect().bottom + (event.key === 'ArrowUp' ? -24 : 24)) }}><span aria-hidden="true">↕</span></div>
}

function importNavigationKind(element: HTMLElement): ImportNavigationTarget | null {
  const value = element.dataset.importNav
  return value === 'queue' || value === 'source' || value === 'match' ? value : null
}

function importControl(element: EventTarget | null): HTMLElement | null {
  return element instanceof Element ? element.closest<HTMLElement>('button, input, select, textarea, [contenteditable], [role="textbox"]') : null
}

function focusFirstImportControl(target: HTMLElement): boolean {
  const control = target.querySelector<HTMLElement>('button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), [contenteditable="true"]')
  if (!control) return false
  control.focus()
  return true
}

function focusWholeAlbumControl(): boolean {
  const control = document.querySelector<HTMLElement>('input[aria-label="Import whole album"]:not(:disabled)')
  if (!control) return false
  control.focus()
  return true
}

function focusImporterTarget(kind: Exclude<ImportNavigationTarget, 'queue'>, row: number): boolean {
  const target = importerTargetElement(kind, row)
  if (!target) return false
  target.focus()
  target.scrollIntoView({ block: 'nearest' })
  return true
}

function importerTargetElement(kind: Exclude<ImportNavigationTarget, 'queue'>, row: number): HTMLElement | null {
  return document.querySelector<HTMLElement>(`[data-import-nav="${kind}"][data-import-row="${row}"]`)
}

function selectedAlbumAdvisory(page: PageView) {
  const selectedItem = page.rows.find((item) => item.matchResult?.selectedUri?.startsWith('spotify:album:'))
  const selectedUri = selectedItem?.matchResult?.selectedUri
  const candidate = selectedItem?.matchResult?.candidates.find((entry) => entry.uri === selectedUri)
  if (!candidate) return { strong: false, extraTrackCount: 0 }
  return strongImportAlbumMatch(page.artist, candidate, page.rows.map((item) => {
    const match = item.matchResult
    const targetUri = match?.trackMatches[item.source.stableId] ?? (match?.selectedUri?.startsWith('spotify:track:') ? match.selectedUri : null)
    const confidence = selectedImportTrackConfidence(item.source.stableId, match?.selectedUri ?? null, match?.trackMatches ?? {}, match?.confidence ?? null, match?.candidates ?? [])
    const standaloneLowConfidence = Boolean(targetUri && confidence === 'low' && match?.candidates.some((entry) => entry.uri === targetUri && entry.uri.startsWith('spotify:track:') && entry.relation === null))
    return { excluded: item.decision.excluded, targetUri, standaloneLowConfidence }
  }))
}

function ImportIntentChecks({ defaults, disabled = false, onChange }: { defaults: LastFmImportDefaults; disabled?: boolean; onChange: (next: LastFmImportDefaults) => void }) {
  const set = (key: 'importContent' | 'includeHistoricalPlayCounts', value: boolean) => {
    const next = { ...defaults, [key]: value }
    if (validImportIntent(next.importContent, next.includeHistoricalPlayCounts)) onChange(next)
  }
  return <fieldset className="import-intent-checks" disabled={disabled}>
    <legend>Choose what Retune should import</legend>
    <label><input type="checkbox" checked={defaults.importContent} disabled={disabled || (!defaults.includeHistoricalPlayCounts && defaults.importContent)} onChange={(event) => set('importContent', event.target.checked)} /><span><strong>Import tracks and albums found in history</strong><small>Add scrobbled Spotify content to Retune.</small></span></label>
    <label><input type="checkbox" checked={defaults.includeHistoricalPlayCounts} disabled={disabled || (!defaults.importContent && defaults.includeHistoricalPlayCounts)} onChange={(event) => set('includeHistoricalPlayCounts', event.target.checked)} /><span><strong>Include historical play counts</strong><small>Apply Last.fm history to matched Retune tracks.</small></span></label>
  </fieldset>
}

function DownloadPane({ state, defaults, busy, onDefaults, onStart }: { state: ImportStateView; defaults: LastFmImportDefaults; busy: boolean; onDefaults: (defaults: LastFmImportDefaults) => void; onStart: () => void }) {
  const isSetup = state.phase === null
  const isSuspended = state.phase === 'suspended'
  const isAggregating = state.phase === 'aggregating'
  const action = downloadAction(state.phase, state.retryableError)
  const copy = importDownloadCopy(state)
  return <section className="import-progress-pane" aria-labelledby="import-progress-title">
    <div className="import-progress-copy">
      <p className="eyebrow">LAST.FM HISTORY</p>
      <h2 id="import-progress-title">{isSetup ? 'Import your complete Last.fm history' : isSuspended ? 'Import suspended for account safety' : isAggregating ? 'Preparing your review queue' : 'Downloading your Last.fm history'}</h2>
      <p>{copy.detail}</p>
      <p className="import-history-breadcrumb">{copy.breadcrumb}</p>
      {!isSetup && !isSuspended && <><progress max={100} value={importDownloadPercent(state.downloadedPages, state.totalPages)} aria-label="Last.fm download progress" /><span className="import-progress-label">{copy.progress}</span></>}
      <ImportIntentChecks defaults={isSetup ? defaults : state.defaults} disabled={!isSetup || busy} onChange={onDefaults} />
      <p className="import-leave-running">You can leave this running — Retune keeps playing, and Spotify is contacted only when you open a review batch.</p>
      {state.retryableError && <p className="import-error" role="alert">{state.retryableError.message} {state.retryableError.retryable ? `Attempt ${state.retryableError.attempt}. Retrying automatically while Retune is running.` : ''}</p>}
      <button type="button" className="primary" disabled={busy || action.disabled} onClick={onStart}>{action.label}</button>
    </div>
  </section>
}

function MatchPickerDialog({ kind, query: initialQuery, candidates, selectedUri, selectedConfidence, busy, onCancel, onSearch, onChoose }: { kind: PickerKind; query: string; candidates: AlbumCandidate[]; selectedUri: string | null; selectedConfidence: MatchResult['confidence']; busy: boolean; onCancel: () => void; onSearch: (query: string) => void; onChoose: (uri: string) => void }) {
  const [query, setQuery] = useState(initialQuery)
  const [choice, setChoice] = useState(selectedUri ?? '')
  useEffect(() => { setQuery(initialQuery) }, [initialQuery])
  useEffect(() => { setChoice(selectedUri ?? '') }, [selectedUri])
  return <ModalDialog className="import-picker-dialog" labelledBy="import-picker-title" onCancel={onCancel} onSubmit={() => { if (choice) void onChoose(choice) }}>
    <header><p className="eyebrow">{kind === 'album' ? 'CHANGE ALBUM' : 'CHANGE TRACK'}</p><h2 id="import-picker-title">{kind === 'album' ? 'Choose a Spotify release' : 'Choose a Spotify track'}</h2></header>
    <div className="import-picker-search"><label htmlFor="import-picker-query">Search Spotify</label><div><input id="import-picker-query" autoFocus value={query} onChange={(event) => setQuery(event.target.value)} onKeyDown={(event) => { if (event.key === 'Enter' && !busy && query.trim()) { event.preventDefault(); onSearch(query) } }} /><button type="button" disabled={busy || !query.trim()} onClick={() => onSearch(query)}>Search</button></div></div>
    <div className="import-picker-results" aria-live="polite">{candidates.length ? candidates.slice(0, 10).map((candidate) => <label className="import-picker-option" key={candidate.uri}><input type="radio" name="import-picker-choice" checked={choice === candidate.uri} onChange={() => setChoice(candidate.uri)} /><span><strong>{candidate.name}</strong><small>{candidate.artist}{kind === 'album' ? ` · ${candidate.trackUris.length} tracks` : candidate.trackAlbums[0] ? ` · ${candidate.trackAlbums[0]}` : ''}</small></span><em>{kind === 'album' ? relationLabel(candidate.relation) : confidenceLabel(candidate.uri === selectedUri && selectedConfidence ? selectedConfidence : candidate.relation === 'best-match' ? 'exact' : candidate.relation ? 'likely' : 'low')}</em></label>) : <p className="muted">Search to load up to 10 real Spotify candidates.</p>}</div>
    {kind === 'album' && <p className="import-picker-note">Counts follow the tracks you keep. Choosing a release remaps this page together.</p>}
    <footer><button type="button" onClick={onCancel}>Cancel</button><button type="submit" className="primary" disabled={busy || !choice}>Use This {kind === 'album' ? 'Album' : 'Track'}</button></footer>
  </ModalDialog>
}

function CollectionAlbumDialog({ page, collection, initialPreviewUri, busy, onCancel, onPage, onError }: { page: PageView; collection: CollectionMatchView; initialPreviewUri?: string; busy: boolean; onCancel: () => void; onPage: (page: PageView) => void; onError: (error: unknown) => void }) {
  const [dialogState, setDialogState] = useState(() => collectionDialogInitialState<AlbumCandidate>(initialPreviewUri))
  const [loading, setLoading] = useState(false)
  const dialog = useRef<HTMLFormElement>(null)
  const resultList = useRef<HTMLDivElement>(null)
  const dialogOffset = useRef({ x: 0, y: 0 })
  const drag = useRef<{ pointerX: number; pointerY: number; offsetX: number; offsetY: number; bounds: DOMRect } | undefined>(undefined)
  const selectedUris = useMemo(() => selectedCollectionAlbumUris(collection.cachedAlbums, collection.selectedAlbumUris), [collection.cachedAlbums, collection.selectedAlbumUris])
  const previewScreen = collectionDialogScreen(dialogState.previewUri, collection.cachedAlbums)
  const preview = dialogState.previewUri ? collection.cachedAlbums.find((candidate) => candidate.uri === dialogState.previewUri) : undefined
  const previewCoverage = preview ? collection.coverage.previews.find((candidate) => candidate.uri === preview.uri) : undefined
  const run = async (action: string, args: Record<string, unknown>) => {
    setLoading(true)
    try {
      const next = await invoke<PageView | AlbumCandidate[] | null>(action, args)
      if (Array.isArray(next)) setDialogState((state) => collectionDialogTransition(state, { type: 'search-succeeded', results: next }))
      else if (next) onPage(next)
      return next
    } catch (error) {
      onError(error)
      return null
    } finally { setLoading(false) }
  }
  const search = () => {
    if (!dialogState.query.trim()) return
    void run('lastfm_import_collection_search_albums', { batchId: page.batchId, artist: page.artist, query: dialogState.query.trim() }).then((next) => {
      if (next === null) setDialogState((state) => collectionDialogTransition(state, { type: 'search-failed' }))
    })
  }
  const openPreview = async (candidate: AlbumCandidate) => {
    setDialogState((state) => collectionDialogTransition(state, {
      type: 'preview-started',
      uri: candidate.uri,
      resultsScrollTop: resultList.current?.scrollTop ?? 0,
    }))
    const next = await run('lastfm_import_collection_preview_album', { batchId: page.batchId, artist: page.artist, uri: candidate.uri })
    setDialogState((state) => collectionDialogTransition(state, next ? { type: 'preview-succeeded', uri: candidate.uri } : { type: 'preview-failed' }))
  }
  const closePreview = (transition: 'back-to-results' | 'preview-add-succeeded' = 'back-to-results') => {
    setDialogState((state) => collectionDialogTransition(state, { type: transition }))
    requestAnimationFrame(() => { if (resultList.current) resultList.current.scrollTop = dialogState.resultsScrollTop })
  }
  const toggleMatch = async () => {
    if (!preview) return
    const selected = selectedUris.includes(preview.uri)
    const action = selected ? 'lastfm_import_collection_remove_album' : 'lastfm_import_collection_add_album'
    const next = await run(action, { batchId: page.batchId, artist: page.artist, uri: preview.uri })
    if (next && !selected) closePreview('preview-add-succeeded')
  }
  const toggleResultMatch = (candidate: AlbumCandidate) => {
    const selected = selectedUris.includes(candidate.uri)
    const action = selected ? 'lastfm_import_collection_remove_album' : 'lastfm_import_collection_add_album'
    return run(action, { batchId: page.batchId, artist: page.artist, uri: candidate.uri })
  }
  const removeMatch = (uri: string) => void run('lastfm_import_collection_remove_album', { batchId: page.batchId, artist: page.artist, uri })
  const dragDialog = (event: ReactPointerEvent<HTMLElement>) => {
    const element = dialog.current
    if (!element) return
    if (event.type === 'pointerdown') {
      if (event.button !== 0) return
      drag.current = { pointerX: event.clientX, pointerY: event.clientY, offsetX: dialogOffset.current.x, offsetY: dialogOffset.current.y, bounds: element.getBoundingClientRect() }
      event.currentTarget.setPointerCapture(event.pointerId)
      return
    }
    if (!event.currentTarget.hasPointerCapture(event.pointerId) || !drag.current) return
    if (event.type === 'pointerup' || event.type === 'pointercancel') {
      event.currentTarget.releasePointerCapture(event.pointerId)
      drag.current = undefined
      return
    }
    const x = drag.current.offsetX + Math.min(window.innerWidth - 80 - drag.current.bounds.left, Math.max(80 - drag.current.bounds.right, event.clientX - drag.current.pointerX))
    const y = drag.current.offsetY + Math.min(window.innerHeight - 50 - drag.current.bounds.top, Math.max(50 - drag.current.bounds.bottom, event.clientY - drag.current.pointerY))
    dialogOffset.current = { x, y }
    element.style.transform = `translate(${x}px, ${y}px)`
  }
  return <ModalDialog className="import-collection-dialog" labelledBy="collection-dialog-title" dialogRef={dialog} onCancel={onCancel} onSubmit={search}>
    <header className="import-dialog-drag-handle" title="Drag to move" onPointerDown={dragDialog} onPointerMove={dragDialog} onPointerUp={dragDialog} onPointerCancel={dragDialog}><p className="eyebrow">COLLECTION ALBUM MATCHES</p><h2 id="collection-dialog-title">Add albums from Spotify</h2><p className="muted">Search one album at a time, preview it, and build the match set without changing library membership.</p></header>
    {preview && previewScreen === 'preview' ? <>
      <button type="button" className="spotify-page-back" onClick={() => closePreview()}>‹ Back to results</button>
      <SpotifyAlbumPresentation album={collectionAlbumPresentation(preview, previewCoverage?.trackStatuses)} compact />
      <p className="import-collection-coverage">{collectionCoverageStatus(collection.coverage)} · {previewCoverage ? collectionPreviewCoverageCopy(previewCoverage) : 'Preview coverage will update after adding'} · {preview.totalTracks ?? preview.trackUris.length} Spotify tracks</p>
      <p className="import-collection-attribution"><a href={`https://open.spotify.com/album/${preview.uri.split(':').pop()}`} target="_blank" rel="noreferrer">Open in Spotify ↗</a> · Track match state is shown above.</p>
      <footer><button type="button" onClick={() => closePreview()}>Back to results</button><button type="button" className="primary" disabled={busy || loading} onClick={() => void toggleMatch()}>{collectionAlbumActionLabel(selectedUris.includes(preview.uri))}</button></footer>
    </> : <>
      <div className="import-picker-search"><label htmlFor="collection-album-query">Search Spotify albums</label><div><input id="collection-album-query" autoFocus value={dialogState.query} onChange={(event) => setDialogState((state) => collectionDialogTransition(state, { type: 'set-query', query: event.target.value }))} /><button type="submit" disabled={busy || loading || !dialogState.query.trim()}>Search</button></div></div>
      <div className="import-collection-coverage" role="status">{collectionCoverageStatus(collection.coverage)}{selectedUris.length ? ` · ${selectedUris.length} selected` : ''}</div>
      <div ref={resultList} className="import-picker-results" aria-live="polite">{dialogState.results.length ? dialogState.results.map((candidate) => { const coverage = collection.coverage.previews.find((entry) => entry.uri === candidate.uri); const selected = selectedUris.includes(candidate.uri); return <article className="import-picker-option" key={candidate.uri} tabIndex={0} aria-label={`${candidate.name} by ${candidate.artist}`} onDoubleClick={() => void openPreview(candidate)} onKeyDown={(event) => { if (event.target !== event.currentTarget || (event.key !== 'Enter' && event.key !== ' ')) return; event.preventDefault(); void openPreview(candidate) }}><span><strong>{candidate.name}</strong><small>{candidate.artist}{candidate.releaseDate ? ` · ${candidate.releaseDate.slice(0, 4)}` : ''} · {candidate.totalTracks ?? candidate.trackUris.length} tracks{coverage ? ` · ${coverage.marginalMatches >= 0 ? '+' : ''}${coverage.marginalMatches} matches` : ''}</small></span><div className="import-picker-actions"><button type="button" disabled={busy || loading} onClick={(event) => { event.stopPropagation(); void openPreview(candidate) }} onDoubleClick={(event) => event.stopPropagation()}>Preview</button><button type="button" disabled={busy || loading} onClick={(event) => { event.stopPropagation(); void toggleResultMatch(candidate) }} onDoubleClick={(event) => event.stopPropagation()}>{collectionAlbumActionLabel(selected)}</button></div></article> }) : <p className="muted">Search to load up to 10 Spotify album summaries.</p>}</div>
      <VerticalResizeHandle target={resultList} label="Resize Spotify search results" minHeight={42} />
      {collection.cachedAlbums.length > 0 && <section className="import-selected-albums"><h3>Selected albums</h3>{selectedUris.map((uri) => { const candidate = collection.cachedAlbums.find((entry) => entry.uri === uri); if (!candidate) return null; const matched = collection.coverage.selectedAlbums.find((entry) => entry.uri === uri); return <article key={uri} className="import-selected-album"><strong>{candidate.name}</strong><small>{candidate.artist} · {matched?.matched ?? 0} matches · {matched?.uniqueCoverage ?? 0} unique</small><button type="button" onClick={() => void openPreview(candidate)}>Preview</button><button type="button" disabled={busy || loading} onClick={() => removeMatch(uri)}>Remove</button></article> })}</section>}
      <footer><button type="button" onClick={onCancel}>Close</button></footer>
    </>}
  </ModalDialog>
}

function AcceptAllDialog({ albumEntities, trackEntities, busy, onCancel, onConfirm }: { albumEntities: number; trackEntities: number; busy: boolean; onCancel: () => void; onConfirm: () => void }) {
  return <ModalDialog className="import-confirm-dialog" labelledBy="import-confirm-title" onCancel={onCancel} onSubmit={onConfirm}>
    <h2 id="import-confirm-title">Accept All Imports…</h2>
    <p>This will save <strong>{trackEntities.toLocaleString()} track {trackEntities === 1 ? 'entity' : 'entities'}</strong> and <strong>{albumEntities.toLocaleString()} album {albumEntities === 1 ? 'entity' : 'entities'}</strong> using the current choices.</p>
    <p className="muted">Undecided rows use their best match. Sum is the default for each fuzzy target unless you chose another strategy. Retune applies this page by page and keeps partial progress if you leave or stop.</p>
    <footer><button type="button" disabled={busy} onClick={onCancel}>Cancel</button><button type="submit" className="primary" disabled={busy}>Accept All Imports</button></footer>
  </ModalDialog>
}

function KeyboardShortcutsDialog({ onCancel }: { onCancel: () => void }) {
  return <ModalDialog className="import-shortcuts-dialog" labelledBy="import-shortcuts-title" onCancel={onCancel}>
    <header><p className="eyebrow">KEYBOARD SHORTCUTS</p><h2 id="import-shortcuts-title">Last.fm importer controls</h2></header>
    <dl className="import-shortcuts-list"><dt>↑ / ↓</dt><dd>Move through the queue or mapping rows</dd><dt>Tab / Shift+Tab</dt><dd>Queue ↔ Last.fm source ↔ Spotify match</dd><dt>Enter</dt><dd>Open a queue batch or enter native controls</dd><dt>E</dt><dd>Change the focused album or track match</dd><dt>Space</dt><dd>Toggle whole-album or track inclusion</dd><dt>X</dt><dd>Exclude or restore the focused source track</dd><dt>S</dt><dd>Skip or resume the focused album</dd><dt>A</dt><dd>Apply selections and advance</dd><dt>Esc</dt><dd>Exit control mode or close a dialog</dd><dt>?</dt><dd>Open this legend</dd></dl>
    <footer><button type="button" onClick={onCancel}>Close</button></footer>
  </ModalDialog>
}

function FuzzyPanel({ rows, targetUri, targetTrack, resultCount, mode, expanded, locked, onMode, onToggle }: { rows: ImportSourceRow[]; targetUri: string; targetTrack?: NonNullable<ReturnType<typeof matchedTrack>> | null; resultCount: number; mode: CountMode; expanded: boolean; locked: boolean; onMode: (mode: CountMode) => void; onToggle: () => void }) {
  const { sourceNameCount, resultCopy } = importCountMergePresentation(rows, mode, resultCount)
  const variants = rows.flatMap((row) => row.variants)
  const headingId = 'import-fuzzy-heading-' + targetUri.replace(/[^a-zA-Z0-9_-]/g, '-')
  const mergeId = headingId + '-details'
  return <section className="import-fuzzy-panel" aria-labelledby={headingId}><div className="import-fuzzy-heading"><h3 id={headingId}>COUNT MERGE</h3><span>{sourceNameCount} Last.fm names → 1 Spotify track · {resultCount.toLocaleString()} resulting plays · choice applies to all count merges</span>{locked && <small className="muted">Locked after import</small>}<button type="button" className="text-button" aria-expanded={expanded} aria-controls={mergeId} onClick={onToggle}>{expanded ? 'Hide merge' : 'Show merge'}</button></div>{expanded && <div id={mergeId} className="import-fuzzy-merge"><section className="import-fuzzy-sources" aria-labelledby={mergeId + '-sources'}><h4 id={mergeId + '-sources'}>Last.fm names</h4><ul>{variants.map((variant, index) => { const name = variant.artist + ' · ' + variant.album + ' · ' + variant.track; const key = [variant.artist, variant.album, variant.track, variant.playCount, variant.earliest, variant.latest, index].join('|'); return <li key={key}><span title={name} aria-label={name}>{name}</span><strong>{variant.playCount.toLocaleString()} plays</strong></li> })}</ul></section><div className="import-fuzzy-connector" aria-hidden="true" /><section className="import-fuzzy-result" aria-labelledby={mergeId + '-result'}><h4 id={mergeId + '-result'}>Spotify track</h4>{targetTrack ? <><strong title={targetTrack.name}>{targetTrack.name}</strong><small>{targetTrack.artist} · {targetTrack.album}</small></> : <strong>{targetUri}</strong>}<output className="import-fuzzy-result-copy" role="status" aria-live="polite" aria-label={resultCount.toLocaleString() + ' plays ' + resultCopy}>{resultCount.toLocaleString()} plays {resultCopy}</output></section></div>}<fieldset disabled={locked} className="import-fuzzy-strategies" aria-label={'Play count strategy for ' + targetUri}><legend>Play counts</legend>{(['sum', 'overwrite', 'zero'] as CountMode[]).map((value) => <label key={value}><input type="radio" name={'fuzzy-' + targetUri} checked={mode === value} onChange={() => onMode(value)} />{value === 'sum' ? 'Sum' : value === 'overwrite' ? 'Use highest' : 'Zero'}</label>)}</fieldset></section>
}

function ImporterRow({ item, rowNumber, checked, needsMatch, collection, selectedTrackUris, ambiguousChoices, fuzzy, fuzzyTarget, fuzzyResultCount, onToggle, onExclude, onChangeTrack, onUseTrack, onFuzzyMode, onFuzzyToggle, fuzzyExpanded, fuzzyMode, fuzzyLocked, showQuery, locked }: { item: PageItem; rowNumber: number; checked: boolean; needsMatch: boolean; collection: boolean; selectedTrackUris: Iterable<string>; ambiguousChoices: CollectionAmbiguousChoice[]; fuzzy?: ImportSourceRow[]; fuzzyTarget?: string; fuzzyResultCount: number; onToggle: () => void; onExclude: () => void; onChangeTrack: () => void; onUseTrack: (uri: string) => void; onFuzzyMode: (mode: CountMode) => void; onFuzzyToggle: () => void; fuzzyExpanded: boolean; fuzzyMode: CountMode; fuzzyLocked: boolean; showQuery: boolean; locked: boolean }) {
  const match = item.matchResult
  const track = matchedTrack(item)
  const displayedSearchTerm = collection && !track ? trackPickerQuery(item.source) : match?.searchTerm
  const suggestion = collection ? collectionSuggestion(item.source, item.matchResult, selectedTrackUris) : null
  const trackConfidence: ImportConfidence = selectedImportTrackConfidence(item.source.stableId, match?.selectedUri ?? null, match?.trackMatches ?? {}, match?.confidence ?? null, match?.candidates ?? [])
  const excluded = item.decision.excluded
  const disabled = locked || excluded || !['pending', 'skipped'].includes(item.decision.status)
  const excludeDisabled = locked || (!excluded && disabled)
  return <article className={`import-track-row${excluded ? ' excluded' : ''}`}>
    <div className="import-source-cell import-nav-target" data-import-nav="source" data-import-row={rowNumber} tabIndex={0} aria-label={`Last.fm source ${item.source.track}`} aria-keyshortcuts={IMPORT_NAV_KEYS}><button type="button" className="import-exclude-glyph" disabled={excludeDisabled} aria-label={excluded ? 'Undo exclusion' : `Exclude ${item.source.track}`} title={excluded ? 'Put this source row back in the queue' : 'Exclude this Last.fm source row'} onClick={onExclude}>{excluded ? '↺' : '⊘'}</button><label className="import-track-check"><input type="checkbox" aria-label={`Include ${item.source.track}`} checked={checked} disabled={disabled} onChange={onToggle} /><span /></label><div className="import-track-copy"><strong>{item.source.track}</strong><small>{item.source.playCount.toLocaleString()} plays · last {new Date(item.source.latest * 1000).toLocaleDateString()}</small>{excluded && <small className="import-excluded-copy">Excluded — won’t be imported or asked about again</small>}{fuzzy && fuzzyTarget && <FuzzyPanel targetTrack={track} resultCount={fuzzyResultCount} rows={fuzzy} targetUri={fuzzyTarget} mode={fuzzyMode} locked={fuzzyLocked || locked} expanded={fuzzyExpanded} onMode={onFuzzyMode} onToggle={onFuzzyToggle} />}</div></div>
    <div className={`import-match-cell import-nav-target${needsMatch ? ' needs-action' : ''}`} data-import-nav="match" data-import-row={rowNumber} tabIndex={0} aria-label={`Spotify match for ${item.source.track}`} aria-keyshortcuts={IMPORT_NAV_KEYS}>{track ? <><strong>{track.name}</strong><small>{track.artist} · {track.album}</small><span className={`confidence ${trackConfidence ?? 'low'}`}>{confidenceLabel(trackConfidence ?? 'low')}</span>{collection && trackConfidence === 'exact' && <span className="import-strong-match">STRONG MATCH</span>}{collection && track.inLibrary && <span className="import-library-badge">ALREADY IN YOUR LIBRARY</span>}</> : ambiguousChoices.length ? <><strong className="import-action-required">Multiple matches</strong><small>Choose the Spotify album for this track.</small><select className="import-ambiguity-select" aria-label={`Choose album match for ${item.source.track}`} value="" disabled={disabled} onChange={(event) => { if (event.target.value) onUseTrack(event.target.value) }}><option value="" disabled>Choose an album…</option>{ambiguousChoices.map((choice) => <option key={choice.uri} value={choice.uri}>{choice.album}{choice.recommended ? ' — recommended' : ''} · {choice.projectedMatches}/{choice.totalTracks} tracks</option>)}</select></> : suggestion ? <><strong>{suggestion.name}</strong><small>{suggestion.artist} · {suggestion.trackAlbums[0] || 'Track result'}</small><span className="import-suggestion-label">SUGGESTED</span><button type="button" className="import-match-action" disabled={disabled} onClick={() => onUseTrack(suggestion.uri)}>Use This Track</button></> : needsMatch ? <><strong className="import-action-required">Action required</strong><small>No supported match</small></> : <small className="muted">No supported match</small>}{showQuery && displayedSearchTerm && <code>q={displayedSearchTerm}</code>}<button type="button" className="text-button" disabled={disabled} onClick={onChangeTrack}>Change Track…</button></div>
  </article>
}

function ImportPage({ page, failed, showQueries, onRefresh, onNext, onApplied, onPrevious, onError, onCollectionPage, onTabToQueue, onShortcuts, onStatus, onMutation }: { page: PageView; failed: boolean; showQueries: boolean; onRefresh: (strict?: boolean) => Promise<ImportQueueItem[]>; onNext: (queue?: ImportQueueItem[], focusQueue?: boolean) => void; onApplied: () => Promise<void>; onPrevious: () => void; onError: (error: unknown) => void; onCollectionPage: (page: PageView) => void; onTabToQueue: () => boolean; onShortcuts: () => void; onStatus: ShortcutStatus; onMutation: (running: boolean) => void }) {
  const [review, setReview] = useState<ReviewState>(() => reviewForPage(page))
  const [busy, setBusy] = useState(false)
  const [applyState, setApplyState] = useState<'ready' | 'enqueueing' | 'loading' | 'error'>('ready')
  const [picker, setPicker] = useState<PickerState | null>(null)
  const [collectionDialogOpen, setCollectionDialogOpen] = useState(false)
  const [collectionPreviewUri, setCollectionPreviewUri] = useState<string>()
  const [selectedAlbumsExpanded, setSelectedAlbumsExpanded] = useState(true)
  const selectedAlbums = useRef<HTMLDetailsElement>(null)
  const [expandedFuzzy, setExpandedFuzzy] = useState<Set<string>>(new Set())
  const [pageError, setPageError] = useState<string>()
  useEffect(() => { setReview(reviewForPage(page)); setExpandedFuzzy(new Set()); setPageError(undefined); if (!page.collection) { setCollectionDialogOpen(false); setCollectionPreviewUri(undefined) } }, [page])
  const persist = async (next: ReviewState, refreshQueue = false) => {
    setReview(next)
    setBusy(true)
    onMutation(true)
    try {
      await invoke('lastfm_import_options', { batchId: page.batchId, artist: page.artist, album: page.album, options: pageOptions(next) })
      if (refreshQueue) await onRefresh()
    } catch (error) { onError(error) } finally { onMutation(false); setBusy(false) }
  }
  const run = async (command: string, args: Record<string, unknown>): Promise<ImportQueueItem[]> => {
    setBusy(true)
    onMutation(true)
    try {
      await invoke(command, args)
      return await onRefresh()
    } catch (error) { onError(error); return [] } finally { onMutation(false); setBusy(false) }
  }
  const runPageMutation = async (command: string, args: Record<string, unknown>): Promise<PageView | null> => {
    setBusy(true)
    onMutation(true)
    try {
      const next = await invoke<PageView | null>(command, args)
      if (next) onCollectionPage(next)
      return next
    } catch (error) { onError(error); return null } finally { onMutation(false); setBusy(false) }
  }
  const reviewableAlbumRows = page.rows.filter((item) => !item.decision.excluded && (item.decision.status === 'pending' || item.decision.status === 'skipped'))
  const canResumeAlbum = reviewableAlbumRows.length > 0 && reviewableAlbumRows.every((item) => item.decision.status === 'skipped')
  const toggleAlbumSkip = async () => {
    const action = canResumeAlbum ? 'restore' : 'skip-album'
    const nextQueue = await run('lastfm_import_review', { batchId: page.batchId, action, artist: page.artist, album: page.album })
    onStatus(action === 'restore' ? 'Album resumed.' : 'Album skipped. Press S again to resume it.')
    if (importAlbumActionAdvances(action)) onNext(nextQueue)
  }
  const apply = async (advance: boolean) => {
    if (!selectedImportCount(review)) {
      onStatus('No selected source tracks to apply.')
      return
    }
    if (requiredMatchIds.length) {
      const first = visibleRows.findIndex((item) => item.source.stableId === requiredMatchIds[0])
      const message = `${requiredMatchIds.length} selected ${requiredMatchIds.length === 1 ? 'track needs a' : 'tracks need'} Spotify match. Change the match or uncheck ${requiredMatchIds.length === 1 ? 'it' : 'them'} before accepting.`
      setPageError(message)
      onStatus(message)
      if (first >= 0) requestAnimationFrame(() => focusImporterTarget('match', first + 1))
      return
    }
    setBusy(true)
    onMutation(true)
    try {
      setPageError(undefined)
      setApplyState('enqueueing')
      await invoke('lastfm_import_apply', { batchId: page.batchId, artist: page.artist, album: page.album, selectedIds: [...review.checked], archiveBatch: advance, options: pageOptions(review) })
      if (advance) setApplyState('loading')
      if (advance) await onApplied()
      else { await onRefresh(true); setApplyState('ready') }
    } catch (error) { setApplyState('error'); setPageError(String(error)); onError(error) } finally { onMutation(false); setBusy(false) }
  }
  const retry = async () => {
    setBusy(true)
    onMutation(true)
    try {
      setPageError(undefined)
      setApplyState('enqueueing')
      await invoke('lastfm_import_retry_apply', { batchId: page.batchId })
      setApplyState('loading')
    } catch (error) {
      setApplyState('error')
      setPageError(String(error))
      onError(error)
    } finally {
      onMutation(false)
      setBusy(false)
    }
  }
  const fuzzyFor = (item: PageItem): { target: string; group: ImportSourceRow[] } | undefined => {
    const target = item.matchResult?.trackMatches[item.source.stableId]
    if (!target) return undefined
    const group = page.fuzzyGroups[target]
    const anchor = group?.find((row) => page.rows.some((entry) => entry.source.stableId === row.stableId))
    if (!group || !anchor || (group.length <= 1 && !group.some((entry) => entry.variants.length > 1)) || anchor.stableId !== item.source.stableId) return undefined
    return { target, group }
  }
  const pickerItem = picker ? page.rows.find((item) => item.source.stableId === picker.sourceId) : undefined
  const pickerMatch = pickerItem?.matchResult
  const openTrackPicker = (sourceId: string) => {
    const item = page.rows.find((entry) => entry.source.stableId === sourceId)
    setPicker({ kind: 'track', sourceId, query: item ? trackPickerQuery(item.source) : '' })
  }
  const openAlbumPicker = () => setPicker({ kind: 'album', sourceId: page.rows[0]?.source.stableId ?? '', query: page.album })
  const openCollectionAlbums = (uri?: string) => { setCollectionPreviewUri(uri); setCollectionDialogOpen(true) }
  const removeCollectionAlbum = async (uri: string) => {
    setBusy(true)
    onMutation(true)
    try {
      const next = await invoke<PageView | null>('lastfm_import_collection_remove_album', { batchId: page.batchId, artist: page.artist, uri })
      if (next) onCollectionPage(next)
    } catch (error) { onError(error) } finally { onMutation(false); setBusy(false) }
  }
  const searchPicker = async (query: string) => {
    if (!picker) return
    const activePicker = picker
    if (activePicker.kind === 'track') await runPageMutation('lastfm_import_change_track', { batchId: page.batchId, id: activePicker.sourceId, query })
    else await run(activePicker.kind === 'album' ? 'lastfm_import_change_album' : 'lastfm_import_change_track', { batchId: page.batchId, id: activePicker.sourceId, query })
    setPicker((current) => current && current.kind === activePicker.kind && current.sourceId === activePicker.sourceId ? { ...current, query } : current)
  }
  const choosePicker = async (uri: string) => {
    if (!picker) return
    const activePicker = picker
    const next = await runPageMutation('lastfm_import_select_match', { batchId: page.batchId, id: activePicker.sourceId, uri })
    if (next) setPicker((current) => current && current.kind === activePicker.kind && current.sourceId === activePicker.sourceId ? null : current)
  }
  const intentChange = (key: 'importContent' | 'includeHistoricalPlayCounts', checked: boolean) => {
    const next = { ...review, [key]: checked }
    if (!validImportIntent(next.importContent, next.includeHistoricalPlayCounts)) return
    if (!next.importContent) next.wholeAlbum = false
    void persist(next, true)
  }
  const fuzzy = (item: PageItem): FuzzyProps => {
    const group = fuzzyFor(item)
    if (!group) return { fuzzyExpanded: false, fuzzyMode: 'sum', fuzzyLocked: false, fuzzyResultCount: 0, onFuzzyMode: () => {}, onFuzzyToggle: () => {} }
    const mode = page.countModes[group.target] ?? 'sum'
    return {
      fuzzy: group.group,
      fuzzyTarget: group.target,
      fuzzyResultCount: page.resolvedCounts[group.target],
      fuzzyMode: mode,
      fuzzyLocked: page.lockedCountModes.includes(group.target),
      fuzzyExpanded: expandedFuzzy.has(group.target),
      onFuzzyMode: (nextMode: CountMode) => void run('lastfm_import_count_mode', { targetUri: group.target, mode: nextMode }),
      onFuzzyToggle: () => setExpandedFuzzy((current) => { const next = new Set(current); if (next.has(group.target)) next.delete(group.target); else next.add(group.target); return next }),
    }
  }
  const handleNavigationKeyDown = (event: ReactKeyboardEvent<HTMLElement>) => {
    const nav = importNavigationTarget(event.target)
    if (!nav) return
    const kind = importNavigationKind(nav)
    if (!kind) return
    const control = importControl(event.target)
    const modal = Boolean((event.target as Element).closest('[role="dialog"]'))
    if (event.key === 'Escape' && control && control !== nav && !modal) {
      event.preventDefault()
      event.stopPropagation()
      nav.focus()
      onStatus('Control mode ended.')
      return
    }
    if (!canHandleImportShortcut({ key: event.key, navigationTarget: kind, control: Boolean(control && control !== nav), modal, altKey: event.altKey, ctrlKey: event.ctrlKey, metaKey: event.metaKey, shiftKey: event.shiftKey })) return
    if (kind === 'queue') return
    const row = Number(nav.dataset.importRow)
    if (!Number.isInteger(row)) return
    const move = event.key === 'ArrowUp' ? -1 : event.key === 'ArrowDown' ? 1 : 0
    if (move) {
      event.preventDefault()
      event.stopPropagation()
      const next = moveImportNavigationRow(row, visibleRows.length + 1, move)
      if (next !== row) {
        focusImporterTarget(kind, next)
      }
      return
    }
    if (event.key === 'Tab') {
      const next = kind === 'source' ? (event.shiftKey ? null : 'match') : (event.shiftKey ? 'source' : null)
      const moved = next ? focusImporterTarget(next, row) : onTabToQueue()
      if (moved) {
        event.preventDefault()
        event.stopPropagation()
      }
      return
    }
    event.preventDefault()
    event.stopPropagation()
    if (event.key === 'Enter') {
      if (!focusFirstImportControl(nav) && !(row === 0 && kind === 'source' && focusWholeAlbumControl())) onStatus('No native control in this mapping target.')
      return
    }
    if (event.key === '?') {
      onShortcuts()
      return
    }
    if (failed) {
      if (event.key.toLowerCase() === 'a') void retry()
      else onStatus('Retry this failed batch before editing it.')
      return
    }
    if (event.key.toLowerCase() === 'e') {
      const item = row === 0 ? undefined : visibleRows[row - 1]
      if (row === 0) openAlbumPicker()
      else if (item && !item.decision.excluded && ['pending', 'skipped'].includes(item.decision.status)) openTrackPicker(item.source.stableId)
      else onStatus('This source track is already done or excluded.')
      return
    }
    if (event.key === ' ') {
      if (row === 0) {
        if (!review.importContent) onStatus('Enable content import before selecting the whole album.')
        else if (collection && !collectionReady) onStatus('Choose exactly one complete Spotify album before importing the collection as a whole album.')
        else void persist({ ...review, wholeAlbum: !review.wholeAlbum }, true)
        return
      }
      const item = visibleRows[row - 1]
      if (item.decision.excluded || !['pending', 'skipped'].includes(item.decision.status)) onStatus('This source track cannot be selected.')
      else void persist(toggleImportRow(review, item.source.stableId), true)
      return
    }
    if (event.key.toLowerCase() === 'x') {
      const item = row === 0 ? undefined : visibleRows[row - 1]
      if (!item) onStatus('Exclude is available on source-track rows.')
      else if (!['pending', 'skipped'].includes(item.decision.status)) onStatus('Done or ignored source tracks cannot be excluded.')
      else void run('lastfm_import_review', { batchId: page.batchId, id: item.source.stableId, action: item.decision.excluded ? 'undo-exclude' : 'exclude', artist: page.artist, album: page.album })
      return
    }
    if (event.key.toLowerCase() === 's') {
      if (!reviewableAlbumRows.length) onStatus('This album is done, ignored, or excluded; nothing changed.')
      else void toggleAlbumSkip()
      return
    }
    if (event.key.toLowerCase() === 'a') void apply(true)
  }
  const selectedAlbumItem = page.rows.find((item) => item.matchResult?.selectedUri?.startsWith('spotify:album:'))
  const selectedAlbumUri = selectedAlbumItem?.matchResult?.selectedUri
  const selectedAlbumCandidate = selectedAlbumItem?.matchResult?.candidates.find((candidate) => candidate.uri === selectedAlbumUri)
  const albumAdvisory = selectedAlbumAdvisory(page)
  const selectedAlbumName = selectedAlbumCandidate?.name ?? 'Choose a release'
  const selectedActionableIds = page.rows.filter((item) => review.checked.has(item.source.stableId) && !item.decision.excluded && (item.decision.status === 'pending' || item.decision.status === 'skipped')).map((item) => item.source.stableId)
  const matchedIds = page.rows.filter((item) => matchedTrack(item)).map((item) => item.source.stableId)
  const requiredMatchIds = requiredImportMatchIds(selectedActionableIds, matchedIds, review.includeHistoricalPlayCounts, review.wholeAlbum)
  const requiredMatches = new Set(requiredMatchIds)
  const collection = page.album === ''
  const collectionMatches = page.collection
  const selectedCollectionUris = collectionMatches ? selectedCollectionAlbumUris(collectionMatches.cachedAlbums, collectionMatches.selectedAlbumUris) : []
  const addedCollectionAlbums = collectionMatches?.cachedAlbums.filter((candidate) => selectedCollectionUris.includes(candidate.uri)) ?? []
  const automaticCollectionAlbums = automaticCollectionAlbumContributors(
    collection ? page.rows.filter((item) => !item.decision.excluded).map(matchedTrack).filter((track) => track !== null) : [],
    addedCollectionAlbums,
  )
  const collectionAlbumReady = !collection || Boolean(selectedAlbumUri && albumAdvisory.strong)
  const collectionWholeAlbumReady = !collection || Boolean(collectionMatches?.wholeAlbumReady)
  // Collection batches do not use the release picker: collection && !collectionAlbumReady.
  const collectionReady = collection ? collectionWholeAlbumReady : collectionAlbumReady
  const wholeAlbumDisabled = busy || failed || !review.importContent || !collectionReady
  const selectedCollectionTrackUris = collectionMatches
    ? new Set(collectionMatches.cachedAlbums.filter((candidate) => selectedCollectionUris.includes(candidate.uri)).flatMap((candidate) => candidate.trackUris))
    : new Set<string>()
  const visibleRows = stablePartitionImportRows(page.rows, requiredMatchIds, (item) => item.source.stableId)
  const summary = collection ? collectionSummary(page, selectedCollectionTrackUris) : null
  const suggestedMatches = collection ? visibleRows.flatMap((item) => {
    if (!review.checked.has(item.source.stableId) || item.decision.excluded || !['pending', 'skipped'].includes(item.decision.status)) return []
    const suggestion = collectionSuggestion(item.source, item.matchResult, selectedCollectionTrackUris)
    return suggestion ? [{ id: item.source.stableId, uri: suggestion.uri }] : []
  }) : []
  const applySuggestedMatches = async () => {
    const next = await runPageMutation('lastfm_import_select_matches', { batchId: page.batchId, selections: suggestedMatches })
    if (next) onStatus(`${suggestedMatches.length} suggested ${suggestedMatches.length === 1 ? 'match' : 'matches'} selected.`)
  }
  // Last.fm supplied no album metadata. Tracks are matched individually.
  return <section className="import-review" aria-labelledby="import-review-title" aria-busy={applyState === 'enqueueing' || applyState === 'loading'} onKeyDown={handleNavigationKeyDown}>
    <header className="import-review-header"><div><p className="eyebrow">{page.artist}</p><h2 id="import-review-title">{page.album || 'Singles'}</h2><p className="import-page-meta">{page.rows.length} source tracks · {page.rows.reduce((total, item) => total + item.source.playCount, 0).toLocaleString()} plays</p>{collection && <><p className="import-collection-note">Last.fm supplied no album metadata. Tracks are matched individually; added Spotify albums can narrow the choices.</p>{summary && <div className="import-collection-summary"><span>{summary.automatic} automatically selected</span><span>{summary.suggested} suggested</span><span>{summary.needsReview} need review</span></div>}</>}</div><div className="import-page-actions"><button type="button" disabled={busy || failed || page.pageNumber <= 1} aria-label="Previous batch" onClick={onPrevious}>‹</button><span>Batch {page.pageNumber} of {page.pageCount}</span><button type="button" disabled={busy || failed || page.pageNumber >= page.pageCount} aria-label="Next batch" onClick={() => onNext()}>›</button>{collection ? <button type="button" disabled={busy || failed} onClick={() => openCollectionAlbums()}>Add albums…</button> : <button type="button" disabled={busy || failed} onClick={openAlbumPicker}>Change Album…</button>}<button type="button" disabled={busy || failed} onClick={() => void toggleAlbumSkip()}>{canResumeAlbum ? 'Resume Album' : 'Skip Album'}</button><button type="button" disabled={busy || failed} onClick={() => void run('lastfm_import_review', { batchId: page.batchId, action: 'ignore-album', artist: page.artist, album: page.album }).then(onNext)}>Ignore Album</button><button type="button" disabled={busy || failed} onClick={() => void run('lastfm_import_review', { batchId: page.batchId, action: 'ignore-artist', artist: page.artist, album: page.album }).then(onNext)}>Ignore Artist</button></div></header>
    <div className="import-album-strip"><div className="import-nav-target" data-import-nav="source" data-import-row="0" tabIndex={0} aria-label="Last.fm album source" aria-keyshortcuts={IMPORT_NAV_KEYS}><p>{collection ? 'WHAT LAST.FM SUPPLIED' : 'WHAT I’M IMPORTING'}</p><strong>{page.album || 'Singles'}</strong><small>{collection ? `${page.artist} · no album metadata · ${page.rows.length} source tracks matched individually` : `${page.artist} · ${page.rows.length} source tracks`}</small></div>{collection ? <div className="import-nav-target" data-import-nav="match" data-import-row="0" tabIndex={0} aria-label="Spotify album matches" aria-keyshortcuts={IMPORT_NAV_KEYS}><p>SPOTIFY ALBUM MATCHES</p><strong>{automaticCollectionAlbums.length || selectedCollectionUris.length ? `${automaticCollectionAlbums.length} automatic · ${selectedCollectionUris.length} added` : 'No album matches yet'}</strong><small>{collectionMatches ? collectionCoverageStatus(collectionMatches.coverage) : 'Search Spotify to build a match set'}</small><button type="button" disabled={busy || failed} onClick={() => openCollectionAlbums()}>Add albums…</button></div> : <div className="import-nav-target" data-import-nav="match" data-import-row="0" tabIndex={0} aria-label="Spotify album match" aria-keyshortcuts={IMPORT_NAV_KEYS}><p>SPOTIFY MATCH</p><strong>{selectedAlbumName}</strong><small>{selectedAlbumCandidate ? relationLabel(selectedAlbumCandidate.relation) : 'No release selected'}</small>{albumAdvisory.strong && <span className="import-strong-match" role="status">STRONG MATCH{albumAdvisory.extraTrackCount ? ` · ${albumAdvisory.extraTrackCount} extra Spotify track${albumAdvisory.extraTrackCount === 1 ? '' : 's'}` : ''}</span>}<button type="button" disabled={busy || failed} onClick={openAlbumPicker}>Change Album…</button></div>}</div>
    {collection && collectionMatches && (automaticCollectionAlbums.length > 0 || selectedCollectionUris.length > 0) && <><details ref={selectedAlbums} className="import-selected-album-cards" open={selectedAlbumsExpanded} onToggle={(event) => setSelectedAlbumsExpanded(event.currentTarget.open)}><summary><strong>{automaticCollectionAlbums.length} automatic · {selectedCollectionUris.length} added</strong><small>{collectionCoverageStatus(collectionMatches.coverage)}</small></summary>{automaticCollectionAlbums.map((album) => <article className="import-selected-album-card" key={`${album.artist}\0${album.name}`}><div className="import-selected-album-art"><span aria-hidden="true">♪</span></div><div className="import-selected-album-copy"><strong>{album.name}</strong><small>{album.artist} · {album.matchCount} automatic library {album.matchCount === 1 ? 'match' : 'matches'}</small></div><span className="import-album-source automatic">AUTOMATIC</span></article>)}{selectedCollectionUris.map((uri) => { const candidate = collectionMatches.cachedAlbums.find((entry) => entry.uri === uri); const coverage = collectionMatches.coverage.selectedAlbums.find((entry) => entry.uri === uri); if (!candidate) return null; const metadata = [candidate.artist, candidate.releaseDate?.slice(0, 4), candidate.albumType].filter(Boolean).join(' · '); return <article className="import-selected-album-card" key={uri}><div className="import-selected-album-art">{candidate.imageUrl ? <img src={candidate.imageUrl} alt="" /> : <span aria-hidden="true">♪</span>}</div><div className="import-selected-album-copy"><strong>{candidate.name}</strong><small>{metadata} · {coverage?.matched ?? 0} matches · {coverage?.uniqueCoverage ?? 0} unique</small></div><span className="import-album-source">ADDED</span><button type="button" disabled={busy || failed} onClick={() => openCollectionAlbums(uri)}>Preview</button><button type="button" disabled={busy || failed} onClick={() => void removeCollectionAlbum(uri)}>Remove</button></article> })}</details>{selectedAlbumsExpanded && <VerticalResizeHandle target={selectedAlbums} label="Resize album matches" minHeight={58} />}</>}
    <div className="import-options" role="group" aria-label="Import options"><label><input type="checkbox" aria-label="Import tracks and albums found in history" checked={review.importContent} disabled={busy || failed || (!review.includeHistoricalPlayCounts && review.importContent)} onChange={(event) => intentChange('importContent', event.target.checked)} /> Import tracks and albums found in history</label><label><input type="checkbox" aria-label="Include historical play counts" checked={review.includeHistoricalPlayCounts} disabled={busy || failed || (!review.importContent && review.includeHistoricalPlayCounts)} onChange={(event) => intentChange('includeHistoricalPlayCounts', event.target.checked)} /> Include historical play counts</label><label><input type="checkbox" aria-label="Import whole album" checked={review.wholeAlbum} disabled={wholeAlbumDisabled} onChange={(event) => void persist({ ...review, wholeAlbum: event.target.checked }, true)} /> Import whole album</label><label>Genre <input aria-label="Import genre" disabled={busy || failed} value={review.genre} onChange={(event) => void persist({ ...review, genre: event.target.value })} placeholder="No change" /></label><label>Rating <select aria-label="Import rating" disabled={busy || failed} value={review.rating ?? ''} onChange={(event) => void persist({ ...review, rating: event.target.value ? Number(event.target.value) : null })}><option value="">No change</option>{[1, 2, 3, 4, 5].map((rating) => <option key={rating} value={rating}>{'★'.repeat(rating)}</option>)}</select></label></div>
    {review.wholeAlbum && <p className="import-exclusion-note">Exclude removes only this Last.fm source row. A track inherently included by the whole album cannot be removed from Spotify here.</p>}
    <div className="import-track-list">{visibleRows.map((item, index) => { const itemFuzzy = fuzzy(item); const ambiguousChoices = collectionMatches ? collectionAmbiguousChoices(item.source.stableId, item.matchResult, collectionMatches.cachedAlbums, selectedCollectionUris, collectionMatches.coverage.selectedAlbums) : []; return <ImporterRow key={item.source.stableId} item={item} rowNumber={index + 1} checked={review.checked.has(item.source.stableId)} needsMatch={requiredMatches.has(item.source.stableId)} collection={collection} selectedTrackUris={selectedCollectionTrackUris} ambiguousChoices={ambiguousChoices} showQuery={showQueries} onToggle={() => void persist(toggleImportRow(review, item.source.stableId), true)} onExclude={() => void run('lastfm_import_review', { batchId: page.batchId, id: item.source.stableId, action: item.decision.excluded ? 'undo-exclude' : 'exclude', artist: page.artist, album: page.album })} onChangeTrack={() => openTrackPicker(item.source.stableId)} onUseTrack={(uri) => void runPageMutation('lastfm_import_select_match', { batchId: page.batchId, id: item.source.stableId, uri })} {...itemFuzzy} fuzzyExpanded={itemFuzzy.fuzzyExpanded ?? false} fuzzyMode={itemFuzzy.fuzzyMode ?? 'sum'} fuzzyLocked={itemFuzzy.fuzzyLocked ?? false} onFuzzyMode={itemFuzzy.onFuzzyMode ?? (() => {})} onFuzzyToggle={itemFuzzy.onFuzzyToggle ?? (() => {})} locked={failed} /> })}</div>
    {pageError && <p className="import-page-error" role="alert">{pageError}</p>}
    <footer className="import-review-footer"><span>{failed ? 'This batch failed and its choices are frozen.' : `${selectedImportCount(review)} selected · ${excludedImportCount(review)} excluded · ${restPendingImportCount(review)} not selected`}</span><div>{failed ? <button type="button" className="primary" disabled={busy} onClick={() => void retry()}>Retry Apply</button> : <>{suggestedMatches.length > 0 && <button type="button" disabled={busy} onClick={() => void applySuggestedMatches()}>Use {suggestedMatches.length} Suggestions</button>}<button type="button" disabled={busy || !selectedImportCount(review)} onClick={() => void apply(false)}>Accept Changes</button><button type="button" className="primary" disabled={busy || !selectedImportCount(review)} onClick={() => void apply(true)}>Accept &amp; Next Batch</button></>}</div></footer>
    {collectionDialogOpen && collectionMatches && <CollectionAlbumDialog page={page} collection={collectionMatches} initialPreviewUri={collectionPreviewUri} busy={busy || failed} onCancel={() => { setCollectionDialogOpen(false); setCollectionPreviewUri(undefined) }} onPage={onCollectionPage} onError={onError} />}
    {picker && pickerItem && <MatchPickerDialog kind={picker.kind} query={picker.query} candidates={pickerCandidates(picker.kind, pickerMatch?.candidates ?? [])} selectedUri={pickerSelectedUri(picker.kind, picker.sourceId, pickerMatch?.selectedUri ?? null, pickerMatch?.trackMatches ?? {})} selectedConfidence={pickerMatch?.confidence ?? null} busy={busy || failed} onCancel={() => setPicker(null)} onSearch={searchPicker} onChoose={choosePicker} />}
  </section>
}

const IMPORT_QUEUE_ROW_HEIGHT = 57
const IMPORT_QUEUE_OVERSCAN = 4

function VirtualQueue({ items, selectedPage, disabled, onOpen, onSkip, onTab, onShortcuts }: { items: ImportQueueItem[]; selectedPage: number | null; disabled: boolean; onOpen: (item: ImportQueueItem) => void; onSkip: (item: ImportQueueItem) => void; onTab: (shiftKey: boolean) => ImportQueueTabTarget | null; onShortcuts: () => void }) {
  const list = useRef<HTMLDivElement>(null)
  const frame = useRef<number | null>(null)
  const focusPending = useRef(false)
  const highlightedPageRef = useRef<number | null>(null)
  const [highlightedIndex, setHighlightedIndex] = useState(0)
  const [viewport, setViewport] = useState({ height: 0, scrollTop: 0 })
  const measure = useCallback(() => {
    const element = list.current
    if (!element) return
    const next = { height: element.clientHeight, scrollTop: element.scrollTop }
    setViewport((current) => {
      const currentRange = importQueueVisibleRange(items.length, current.scrollTop, current.height, IMPORT_QUEUE_ROW_HEIGHT, IMPORT_QUEUE_OVERSCAN)
      const nextRange = importQueueVisibleRange(items.length, next.scrollTop, next.height, IMPORT_QUEUE_ROW_HEIGHT, IMPORT_QUEUE_OVERSCAN)
      return currentRange.start === nextRange.start && currentRange.end === nextRange.end && currentRange.contentHeight === nextRange.contentHeight ? current : next
    })
  }, [items.length])
  const scheduleMeasure = useCallback(() => {
    if (frame.current !== null) return
    frame.current = window.requestAnimationFrame(() => {
      frame.current = null
      measure()
    })
  }, [measure])
  useEffect(() => {
    const element = list.current
    if (!element) return
    const observer = new ResizeObserver(scheduleMeasure)
    observer.observe(element)
    element.addEventListener('scroll', scheduleMeasure, { passive: true })
    scheduleMeasure()
    return () => {
      observer.disconnect()
      element.removeEventListener('scroll', scheduleMeasure)
      if (frame.current !== null) {
        window.cancelAnimationFrame(frame.current)
        frame.current = null
      }
    }
  }, [scheduleMeasure])
  const range = useMemo(() => importQueueVisibleRange(items.length, viewport.scrollTop, viewport.height, IMPORT_QUEUE_ROW_HEIGHT, IMPORT_QUEUE_OVERSCAN), [items.length, viewport])
  useEffect(() => {
    setHighlightedIndex((current) => {
      const next = importQueueHighlightIndex(items, highlightedPageRef.current, selectedPage, current)
      highlightedPageRef.current = items[next]?.page ?? null
      return next
    })
  }, [items, selectedPage])
  useLayoutEffect(() => {
    const element = list.current
    if (!element || selectedPage === null) return
    const index = items.findIndex((item) => item.page === selectedPage)
    if (index < 0) return
    const top = index * IMPORT_QUEUE_ROW_HEIGHT
    const bottom = top + IMPORT_QUEUE_ROW_HEIGHT
    const visibleBottom = element.scrollTop + element.clientHeight
    const nextScrollTop = top < element.scrollTop ? top : bottom > visibleBottom ? bottom - element.clientHeight : null
    if (nextScrollTop !== null && nextScrollTop !== element.scrollTop) {
      element.scrollTop = Math.max(0, nextScrollTop)
      scheduleMeasure()
    }
  }, [items, scheduleMeasure, selectedPage])
  useLayoutEffect(() => {
    if (!focusPending.current) return
    const element = list.current
    if (!element) return
    const top = highlightedIndex * IMPORT_QUEUE_ROW_HEIGHT
    const bottom = top + IMPORT_QUEUE_ROW_HEIGHT
    const visibleBottom = element.scrollTop + element.clientHeight
    if (top < element.scrollTop || bottom > visibleBottom) {
      element.scrollTop = top < element.scrollTop ? top : bottom - element.clientHeight
      scheduleMeasure()
    }
    const target = element.querySelector<HTMLElement>(`[data-queue-index="${highlightedIndex}"]`)
    if (target) {
      focusPending.current = false
      target.focus()
    }
  }, [highlightedIndex, range, scheduleMeasure])
  const handleKeyDown = (event: ReactKeyboardEvent<HTMLButtonElement>, item: ImportQueueItem, index: number) => {
    const nav = importNavigationTarget(event.target)
    if (!nav || importNavigationKind(nav) !== 'queue') return
    const control = importControl(event.target)
    const modal = Boolean((event.target as Element).closest('[role="dialog"]'))
    if (!canHandleImportShortcut({ key: event.key, navigationTarget: 'queue', control: Boolean(control && control !== nav), modal, altKey: event.altKey, ctrlKey: event.ctrlKey, metaKey: event.metaKey, shiftKey: event.shiftKey })) return
    if (event.key === 'ArrowUp' || event.key === 'ArrowDown') {
      event.preventDefault()
      event.stopPropagation()
      const next = moveImportQueueIndex(index, items.length, event.key === 'ArrowUp' ? -1 : 1)
      if (next !== index) {
        focusPending.current = true
        highlightedPageRef.current = items[next]?.page ?? null
        setHighlightedIndex(next)
      }
      return
    }
    if (event.key === 'Tab') {
      const target = onTab(event.shiftKey)
      handleImportQueueTab(event, target)
      return
    }
    event.preventDefault()
    event.stopPropagation()
    if (event.key === 'Enter') onOpen(item)
    else if (event.key.toLowerCase() === 's') onSkip(item)
    else if (event.key === '?') onShortcuts()
  }
  return <div ref={list} className="import-queue-list" aria-label="Import queue">
    <div className="import-queue-canvas" style={{ height: range.contentHeight }}>
      <div className="import-queue-window" style={{ transform: `translateY(${range.offsetTop}px)` }}>
        {items.slice(range.start, range.end).map((item, index) => { const absoluteIndex = range.start + index; return <button type="button" data-import-nav="queue" data-queue-index={absoluteIndex} data-highlighted={highlightedIndex === absoluteIndex ? 'true' : undefined} aria-current={selectedPage === item.page ? 'true' : undefined} aria-keyshortcuts="ArrowUp ArrowDown Tab Shift+Tab Enter S ?" tabIndex={highlightedIndex === absoluteIndex ? 0 : -1} aria-label={`Batch ${absoluteIndex + 1} of ${items.length}: ${item.album || 'Singles'} by ${item.artist}, ${item.playCount.toLocaleString()} plays`} disabled={disabled} className={`import-queue-row${selectedPage === item.page ? ' selected' : ''}${highlightedIndex === absoluteIndex ? ' highlighted' : ''}`} key={item.page} onFocus={() => { highlightedPageRef.current = item.page; setHighlightedIndex((current) => current === absoluteIndex ? current : absoluteIndex) }} onKeyDown={(event) => handleKeyDown(event, item, absoluteIndex)} onClick={() => { highlightedPageRef.current = item.page; setHighlightedIndex(absoluteIndex); onOpen(item) }}><span className={`import-status-dot ${item.status ?? 'pending'}`} aria-label={item.status ?? 'pending'} title={item.error ?? undefined}>{item.status === 'done' ? '✓' : item.status === 'skipped' ? '–' : item.status === 'failed' ? '!' : item.status === 'excluded' || item.status?.startsWith('ignored') ? '⊘' : '•'}</span><span className="import-queue-copy"><strong>{item.album || 'Singles'}</strong><small>{item.error ? `Apply failed: ${item.error}` : `${item.artist} · ${item.sourceCount} tracks`}</small></span><span className="import-queue-count">{item.playCount.toLocaleString()}<small>plays</small></span></button> })}
      </div>
    </div>
  </div>
}

export default function LastFmImporter() {
  const [state, setState] = useState(emptyState)
  const [queue, setQueue] = useState<ImportQueueItem[]>([])
  const [sort, setSort] = useState<'plays' | 'artist' | 'batch' | 'lastPlayed'>('plays')
  const [showQueries, setShowQueries] = useState(true)
  const [selected, setSelected] = useState<ImportQueueItem | null>(null)
  const [page, setPage] = useState<PageView | null>(null)
  const [busy, setBusy] = useState(false)
  const [pageLoading, setPageLoading] = useState(false)
  const [error, setError] = useState<string>()
  const [errorRetryAt, setErrorRetryAt] = useState<number | null>()
  const [acceptAllOpen, setAcceptAllOpen] = useState(false)
  const [acceptAllSummary, setAcceptAllSummary] = useState<{ albumEntities: number; trackEntities: number } | null>(null)
  const [shortcutsOpen, setShortcutsOpen] = useState(false)
  const [shortcutStatus, setShortcutStatus] = useState('')
  const [pendingDefaults, setPendingDefaults] = useState<LastFmImportDefaults>(emptyDefaults)
  const pageRequestGeneration = useRef(0)
  const queueRefreshGeneration = useRef(0)
  const acceptAllRunning = useRef(false)
  const queueMutationRunning = useRef(false)
  const focusQueueAfterOpen = useRef(false)
  const advancingApply = useRef(false)
  const prefetchedTransition = useRef('')
  const orderedQueue = useMemo(() => sortImportQueue(queue, sort), [queue, sort])
  const activeQueue = useMemo(() => activeImportQueue(orderedQueue), [orderedQueue])
  const queueSummary = useMemo(() => {
    let plays = 0
    let remaining = 0
    for (const item of queue) {
      if (item.remaining) {
        plays += item.playCount
        remaining += 1
      }
    }
    return { plays, remaining, reviewed: queue.length - remaining }
  }, [queue])
  const selectedPage = selected?.page
  const selectedPageRef = useRef<number | undefined>(undefined)
  selectedPageRef.current = selectedPage
  const sortRef = useRef<'plays' | 'artist' | 'batch' | 'lastPlayed'>('plays')
  sortRef.current = sort
  const refresh = useCallback(async (strict = false): Promise<ImportQueueItem[]> => {
    const requestGeneration = ++pageRequestGeneration.current
    const currentSelectedPage = selectedPageRef.current
    const currentSort = sortRef.current
    try {
      const [nextState, nextQueue] = await Promise.all([invoke<ImportStateView>('lastfm_import_state'), loadImportQueue()])
      if (!isCurrentImportPageResponse(requestGeneration, pageRequestGeneration.current)) return nextQueue
      setState(nextState)
      setShowQueries(nextState.searchTerms)
      setQueue(nextQueue)
      setPendingDefaults(nextState.defaults)
      const orderedNextQueue = sortImportQueue(nextQueue, currentSort)
      const activeNextQueue = activeImportQueue(orderedNextQueue)
      const current = currentSelectedPage === undefined ? undefined : activeNextQueue.find((item) => item.page === currentSelectedPage)
      const firstRemaining = activeNextQueue[0]
      const target = current ?? ((nextState.phase === 'review' || nextState.phase === 'done') ? firstRemaining : undefined)
      if (target) {
        if (target.page !== currentSelectedPage) setSelected(target)
        await applyCurrentImportPageResponse(requestGeneration, () => pageRequestGeneration.current, invoke<PageView | null>('lastfm_import_page', { batchId: target.page, artist: target.artist, album: target.album }), (nextPage) => setPage(pageWithQueuePosition(nextPage, activeNextQueue)))
      } else if (isCurrentImportPageResponse(requestGeneration, pageRequestGeneration.current) && nextState.phase !== 'review' && nextState.phase !== 'done') {
        setSelected(null)
        setPage(null)
      }
      return nextQueue
    } catch (reason) {
      if (isCurrentImportPageResponse(requestGeneration, pageRequestGeneration.current)) setError(String(reason))
      if (strict && isCurrentImportPageResponse(requestGeneration, pageRequestGeneration.current)) throw reason
      return []
    }
  }, [])
  const focusQueueTarget = () => {
    const target = document.querySelector<HTMLElement>('[data-import-nav="queue"][data-highlighted="true"]')
    if (!target) return false
    target.focus()
    target.scrollIntoView({ block: 'nearest' })
    return true
  }
  const focusMappingFromQueue = (shiftKey: boolean): ImportQueueTabTarget | null => {
    const target = importQueueTabTarget(shiftKey)
    return importerTargetElement(target.kind, target.row)
  }
  const refreshQueueOnly = async (strict = false): Promise<ImportQueueItem[]> => {
    const requestGeneration = ++queueRefreshGeneration.current
    try {
      const [nextState, nextQueue] = await Promise.all([invoke<ImportStateView>('lastfm_import_state'), loadImportQueue()])
      if (requestGeneration !== queueRefreshGeneration.current) return nextQueue
      setState(nextState)
      setShowQueries(nextState.searchTerms)
      setQueue(nextQueue)
      setPendingDefaults(nextState.defaults)
      return nextQueue
    } catch (reason) {
      if (requestGeneration !== queueRefreshGeneration.current) return []
      setError(String(reason))
      if (strict) throw reason
      return []
    }
  }
  const skipQueueItem = async (item: ImportQueueItem) => {
    if (!item.remaining) {
      setShortcutStatus(item.status === 'done' ? 'That batch is already done.' : 'That batch is already ignored or excluded.')
      return
    }
    const action = item.status === 'skipped' ? 'restore' : 'skip-album'
    queueMutationRunning.current = true
    setBusy(true)
    try {
      await invoke<ImportStateView>('lastfm_import_review', { batchId: item.page, action, artist: item.artist, album: item.album })
      await refreshQueueOnly()
      setShortcutStatus(action === 'restore' ? 'Album resumed.' : 'Album skipped. Press S again to resume it.')
    } catch (reason) {
      setError(String(reason))
    } finally {
      queueMutationRunning.current = false
      setBusy(false)
    }
  }
  useEffect(() => {
    void refresh()
    const subscription = listen<ImportStateView>('lastfm-import-changed', () => { if (shouldRefreshImportEvent(acceptAllRunning.current, queueMutationRunning.current)) void refresh() })
    const completions = listen<{ batchId?: number; message?: string; retryAt?: number | null }>('lastfm-import-apply-finished', (event) => {
      if (event.payload.message) {
        setError(event.payload.message)
        setErrorRetryAt(event.payload.retryAt)
        void refreshQueueOnly()
      } else if (!advancingApply.current && event.payload.batchId === selectedPageRef.current) {
        setError(undefined)
        setErrorRetryAt(undefined)
        void refresh()
      }
    })
    return () => { void subscription.then((stop) => stop()); void completions.then((stop) => stop()) }
  }, [refresh])
  useEffect(() => { setPage((current) => pageWithQueuePosition(current, activeQueue)) }, [activeQueue])
  useEffect(() => {
    if (!page || pageLoading || selected?.page !== page.batchId) return
    const next = nextRemainingImportQueue(queue, selected, sort)
    if (!next) return
    const transition = `${page.batchId}:${next.page}`
    if (prefetchedTransition.current === transition) return
    prefetchedTransition.current = transition
    // ponytail: one-batch lookahead; widen only if measured navigation still stalls.
    void invoke('lastfm_import_page', { batchId: next.page, artist: next.artist, album: next.album }).catch(() => {
      if (prefetchedTransition.current === transition) prefetchedTransition.current = ''
    })
  }, [page, pageLoading, queue, selected, sort])
  useEffect(() => {
    const media = window.matchMedia('(prefers-color-scheme: dark)')
    let theme: Settings['theme'] = 'system'
    const apply = (next: Settings['theme']) => { theme = next; document.documentElement.dataset.theme = next === 'system' ? media.matches ? 'dark' : 'light' : next }
    const onMediaChange = () => apply(theme)
    apply('system')
    media.addEventListener('change', onMediaChange)
    const subscription = listen<Settings>('settings-changed', (event) => apply(event.payload.theme))
    void invoke<Settings>('get_settings').then((settings) => apply(settings.theme)).catch(() => {})
    return () => { media.removeEventListener('change', onMediaChange); void subscription.then((stop) => stop()) }
  }, [])
  useEffect(() => { getCurrentWindow().setTitle('Last.fm importer').catch(() => {}) }, [])
  const start = async () => {
    if (!validImportIntent(pendingDefaults.importContent, pendingDefaults.includeHistoricalPlayCounts)) return
    setBusy(true); setError(undefined)
    try { await invoke('start_lastfm_import', { defaults: pendingDefaults }); await refresh() } catch (reason) { setError(String(reason)) } finally { setBusy(false) }
  }
  const openQueueItem = async (item: ImportQueueItem, queueSnapshot = activeQueue, focusQueue = false) => {
    setError(undefined)
    setErrorRetryAt(undefined)
    focusQueueAfterOpen.current = focusQueue
    const requestGeneration = pageRequestGeneration.current + 1
    setPageLoading(true)
    try {
      await loadSelectedImportPage(pageRequestGeneration, item, (target) => invoke<PageView | null>('lastfm_import_page', { batchId: target.page, artist: target.artist, album: target.album }), setSelected, (nextPage) => setPage(pageWithQueuePosition(nextPage, queueSnapshot)), () => setPage(null), () => setPageLoading(false))
    } catch (reason) {
      focusQueueAfterOpen.current = false
      if (isCurrentImportPageResponse(requestGeneration, pageRequestGeneration.current)) setError(String(reason))
    }
  }
  useLayoutEffect(() => {
    if (!focusQueueAfterOpen.current || pageLoading || !selected) return
    const target = document.querySelector<HTMLElement>(`[data-import-nav="queue"][aria-current="true"]`)
    if (!target) return
    focusQueueAfterOpen.current = false
    target.focus()
    target.scrollIntoView({ block: 'nearest' })
  }, [activeQueue, pageLoading, selected])
  const nextQueueItem = (queueSnapshot = queue, focusQueue = false) => {
    const orderedSnapshot = sortImportQueue(queueSnapshot, sort)
    const next = nextRemainingImportQueue(orderedSnapshot, selected, sort)
    if (next) {
      void openQueueItem(next, activeImportQueue(orderedSnapshot), focusQueue)
    }
    else { setSelected(null); setPage(null); void refresh() }
  }
  const appliedAndAdvance = async () => {
    focusQueueAfterOpen.current = true
    advancingApply.current = true
    const appliedPage = selected?.page
    if (appliedPage === undefined) {
      advancingApply.current = false
      return
    }
    const projection = projectAcknowledgedImportApply(queue, appliedPage, sort)
    setQueue(projection.queue)
    setSelected(null)
    setPage(null)
    try {
      if (projection.next) {
        await openQueueItem(projection.next, activeImportQueue(sortImportQueue(projection.queue, sort)), true)
      } else {
        focusQueueAfterOpen.current = false
      }
    } finally {
      advancingApply.current = false
    }
    void refreshQueueOnly(true).catch(() => {})
  }
  const previousQueueItem = () => {
    const index = selected ? orderedQueue.findIndex((item) => item.page === selected.page) : orderedQueue.length
    const previous = orderedQueue.slice(0, index).reverse().find((item) => item.remaining) ?? orderedQueue.slice(index + 1).reverse().find((item) => item.remaining)
    if (previous) void openQueueItem(previous)
  }
  const acceptAll = async () => {
    setBusy(true); setError(undefined); acceptAllRunning.current = true
    try {
      const nextState = await invoke<ImportStateView>('lastfm_import_accept_all')
      setState(nextState)
      setAcceptAllOpen(false)
      setAcceptAllSummary(null)
      void refreshQueueOnly()
    } catch (reason) { setError(String(reason)) } finally { acceptAllRunning.current = false; setBusy(false) }
  }
  const prepareAcceptAll = async () => {
    setBusy(true); setError(undefined); acceptAllRunning.current = true
    try {
      const summary = await invoke<{ albumEntities: number; trackEntities: number }>('lastfm_import_prepare_accept_all')
      await refresh()
      setAcceptAllSummary(summary)
      setAcceptAllOpen(true)
    } catch (reason) { setError(String(reason)) } finally { acceptAllRunning.current = false; setBusy(false) }
  }
  const setSearchTerms = async (show: boolean) => {
    setShowQueries(show)
    setBusy(true)
    try { await invoke('lastfm_import_search_terms', { show }) } catch (reason) { setError(String(reason)) } finally { setBusy(false) }
  }
  const reviewReady = state.phase === 'review' || state.phase === 'done'
  const emptyPage = importEmptyPageMessage(state.phase, pageLoading)
  return <main className="lastfm-importer" aria-label="Last.fm importer">
    <header className="import-toolbar"><div><p className="eyebrow">LAST.FM HISTORY</p><h1>Last.fm importer</h1><p className="import-status" aria-live="polite">{state.applyingAll ? 'Applying confirmed Last.fm imports' : state.syncing ? 'Syncing new Last.fm plays' : importStatusText(state.phase, state.username)}{showsImportRemaining(state.phase) && state.remaining ? ` · ${state.remaining.toLocaleString()} left` : ''}{state.pendingReview && !state.remaining ? ` · ${state.pendingReview.toLocaleString()} pending review` : ''}</p></div><div className="import-toolbar-actions"><a href="https://www.last.fm/" target="_blank" rel="noreferrer">Powered by Last.fm</a><button type="button" aria-keyshortcuts="?" disabled={acceptAllOpen} onClick={() => setShortcutsOpen(true)}>Keyboard shortcuts (?)</button>{reviewReady && <><span className="import-sort-label">Sort</span><div className="import-sort-control" role="group" aria-label="Queue sort">{([['plays', 'Most played'], ['artist', 'Artist A–Z'], ['batch', 'Batch size'], ['lastPlayed', 'Last played']] as const).map(([value, label]) => <button type="button" key={value} aria-pressed={sort === value} className={sort === value ? 'active' : ''} onClick={() => setSort(value)}>{label}</button>)}</div><label className="import-query-toggle"><input type="checkbox" aria-label="Show Spotify search terms" checked={showQueries} disabled={busy} onChange={(event) => void setSearchTerms(event.target.checked)} /> Show Spotify search terms</label><button type="button" disabled={busy || state.applyingAll || !state.remaining} onClick={() => void prepareAcceptAll()}>Accept All Imports…</button></>}</div></header>
    {(error ?? selected?.error) ? <div className="import-error" role="alert"><span>{error ?? selected?.error}</span><SpotifyLimitNotice message={(error ?? selected?.error)!} retryAt={error ? errorRetryAt : selected?.retryAt} /></div> : state.spotifyLimit && <div className="import-limit" role="status"><span>{state.spotifyLimit.kind === 'quota' ? 'Spotify Development Mode quota is cooling down.' : 'Spotify is rate limited.'}</span><SpotifyLimitNotice message={state.spotifyLimit.kind === 'quota' ? 'Spotify Development Mode quota exhausted' : 'Spotify rate limited'} retryAt={state.spotifyLimit.deadline} /></div>}
    {state.phase === 'downloading' || state.phase === 'aggregating' || state.phase === null || state.phase === 'suspended' ? <DownloadPane state={state} defaults={pendingDefaults} busy={busy} onDefaults={setPendingDefaults} onStart={() => void start()} /> : <div className="import-workspace" aria-busy={pageLoading || state.applyingAll}><aside className="import-queue" aria-label="Import queue"><div className="import-queue-header"><div><h2>Import queue</h2><small>{queueSummary.remaining} batches · {queueSummary.plays.toLocaleString()} plays</small></div><span>{queueSummary.remaining} left</span></div><VirtualQueue items={activeQueue} selectedPage={selected?.page ?? null} disabled={busy || state.applyingAll} onOpen={(item) => void openQueueItem(item, activeQueue, true)} onSkip={(item) => void skipQueueItem(item)} onTab={focusMappingFromQueue} onShortcuts={() => setShortcutsOpen(true)} /><div className="import-queue-progress"><progress max={queue.length || 1} value={queueSummary.reviewed} aria-label="Reviewed queue progress" /><span>Reviewed {queueSummary.reviewed} of {queue.length} batches</span></div></aside>{state.applyingAll ? <section className="import-empty"><strong>Applying confirmed imports…</strong><span>You can close this window; Retune will resume the queue after a restart.</span></section> : page ? <ImportPage page={page} failed={selected?.status === 'failed'} showQueries={showQueries} onRefresh={refresh} onNext={nextQueueItem} onApplied={appliedAndAdvance} onPrevious={previousQueueItem} onError={(reason) => setError(String(reason))} onCollectionPage={(nextPage) => setPage(pageWithQueuePosition(nextPage, activeQueue))} onTabToQueue={focusQueueTarget} onShortcuts={() => setShortcutsOpen(true)} onStatus={setShortcutStatus} onMutation={(running) => { queueMutationRunning.current = running }} /> : <section className="import-empty"><strong>{emptyPage.title}</strong><span>{emptyPage.detail}</span></section>}</div>}
    <footer className="import-footer"><span>Historical import is an absolute baseline; incremental sync adds new plays, deduplicates Retune-origin scrobbles locally, and never erases existing plays.</span><span className="import-footer-hints">↑↓ move · Tab columns · Enter controls · E edit · Space toggle · X exclude · S skip/resume · A apply · ? shortcuts</span><span role="status" aria-live="polite">{shortcutStatus || (state.username ? `Last.fm: ${state.username}` : 'Account not connected')}</span></footer>
    {acceptAllOpen && acceptAllSummary && <AcceptAllDialog albumEntities={acceptAllSummary.albumEntities} trackEntities={acceptAllSummary.trackEntities} busy={busy} onCancel={() => { setAcceptAllOpen(false); setAcceptAllSummary(null) }} onConfirm={() => void acceptAll()} />}
    {shortcutsOpen && <KeyboardShortcutsDialog onCancel={() => setShortcutsOpen(false)} />}
  </main>
}
