import { listen } from '@tauri-apps/api/event'
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent, type MouseEvent as ReactMouseEvent, type PointerEvent as ReactPointerEvent, type RefObject } from 'react'
import { flushSync } from 'react-dom'
import { AutocompleteInput } from './dialogViews.tsx'
import { SpotifyAlbumPresentation, type SpotifyAlbumPresentationData } from './spotifyViews.tsx'
import { ModalDialog } from './viewShared.tsx'
import type { Appearance, LastFmImportDefaults, LastFmImportState } from './types.ts'
import { libraryGateway } from './libraryGateway.ts'
import { lastfmEvents, lastfmGateway, type AlbumCandidate, type CollectionMatchView, type ImportPageOptions, type MatchResult, type PageItem, type PageView, type ReviewAction } from './lastfmGateway.ts'
import { openExternalDestination, subscribeThenSnapshot, subscriptionsThenSnapshot } from './ipc.ts'
import { appGateway } from './appGateway.ts'
import { activeImportQueue, applyCurrentImportPageResponse, applyCurrentImportRefresh, beginImportRefresh, canHandleImportShortcut, collectionAlbumActionLabel, collectionAlbumTrackStatuses, collectionAmbiguousChoices, collectionCoverageStatus, collectionDialogInitialState, collectionDialogScreen, collectionDialogTransition, collectionPreviewCoverageCopy, collectionSuggestion, downloadAction, excludeImportRows, excludedImportCount, filterImportQueue, handleImportQueueTab, importAlbumActionAdvances, importApplyErrorCode, importCountMergePresentation, importDownloadCopy, importDownloadPercent, importEmptyPageMessage, importQueueHighlightIndex, importQueueTabTarget, importQueueVisibleRange, importStatusText, isCurrentImportPageResponse, isCurrentImportRefresh, loadSelectedImportPage, mergeReviewBatchDraft, moveImportNavigationRow, moveImportQueueIndex, nextRemainingImportQueue, parseImportApplyResult, pickerCandidates, pickerSelectedUri, projectAcknowledgedImportApply, projectImportQueueExclusion, requiredImportMatchIds, restPendingImportCount, runCheckedImportMutation, selectImportRows, selectedCollectionAlbumUris, selectedImportCount, selectedImportTrackConfidence, setWholeAlbumImport, shouldRefreshImportEvent, showsImportRemaining, sortImportQueue, spotifyLimitCountdown, stablePartitionImportRows, strongImportAlbumMatch, toggleImportRow, trackPickerQuery, validImportIntent, type CollectionAmbiguousChoice, type CollectionTrackStatusProjection, type CountMode, type ImportApplyErrorCode, type ImportConfidence, type ImportNavigationTarget, type ImportPickerKind, type ImportQueueItem, type ImportQueueTabTarget, type ImportRowSelection, type ImportSourceRow, type ReviewBatchKey, type ReviewState } from './lastfmImportState.ts'
import './lastfmImporter.css'

type ImportStateView = LastFmImportState
type PickerKind = ImportPickerKind
type PickerState = { kind: PickerKind; sourceId: string; sourceIds: string[]; query: string }
type FuzzyProps = { fuzzy?: ImportSourceRow[]; fuzzyTarget?: string; fuzzyResultCount: number; fuzzyExpanded: boolean; fuzzyMode: CountMode; fuzzyLocked: boolean; onFuzzyMode: (mode: CountMode) => void; onFuzzyToggle: () => void }
type ShortcutStatus = (message: string) => void

const IMPORT_NAV_KEYS = 'ArrowUp ArrowDown Tab Shift+Tab Enter E Space X S A Escape ?'

const emptyDefaults: LastFmImportDefaults = { importContent: true, includeHistoricalPlayCounts: true, wholeAlbum: false }
const emptyState: ImportStateView = { phase: null, username: null, spotifyAccountId: null, historyTo: null, downloadedThrough: null, nextPage: 1, totalPages: null, downloadedPages: 0, totalScrobbles: 0, includedScrobbles: 0, processedScrobbles: 0, defaults: emptyDefaults, remaining: 0, retryableError: null, searchTerms: true, syncing: false, lastSyncedAt: null, pendingReview: 0, syncProblem: 'Retune is still loading Last.fm import state.', applyingAll: false, spotifyLimit: null }
const importQueuePageLimit = 1000
const invalidApplyResultMessage = 'Retune received an invalid Last.fm import result.'
type DisplayError = { message: string; code: ImportApplyErrorCode; retryAt: number | null }

function SpotifyLimitNotice({ code, retryAt }: { code: ImportApplyErrorCode; retryAt?: number | null }) {
  const limited = code === 'spotify-rate-limited' || code === 'spotify-quota-exhausted'
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
  for (let attempt = 0; attempt < 2; attempt += 1) {
    const items: ImportQueueItem[] = []
    let cursor = 0
    let total: number | undefined
    while (true) {
      const page = await lastfmGateway.queue(cursor, importQueuePageLimit)
      if (page.cursor !== cursor || page.items.length > importQueuePageLimit) throw new Error('Last.fm import queue pagination is invalid.')
      if (total !== undefined && page.total !== total) break
      total ??= page.total
      items.push(...page.items)
      if (page.nextCursor === null) {
        if (items.length === page.total) return items
        break
      }
      if (!Number.isSafeInteger(page.nextCursor) || page.nextCursor <= cursor || page.nextCursor > page.total) throw new Error('Last.fm import queue pagination is invalid.')
      cursor = page.nextCursor
    }
  }
  throw new Error('Last.fm import queue changed while it was loading. Please retry.')
}

function reviewForPage(page: PageView): ReviewState {
  const rows = page.rows.map((item) => item.source)
  const decisions = Object.fromEntries(page.rows.map((item) => [item.source.stableId, item.decision]))
  const review = {
    rows,
    decisions,
    checked: new Set(page.options.selectedTrackIds),
    importContent: page.options.importContent,
    includeHistoricalPlayCounts: page.options.includeHistoricalPlayCounts,
    wholeAlbum: page.options.wholeAlbum,
    genre: page.options.genre ?? '',
    rating: page.options.rating,
  }
  return setWholeAlbumImport(review, review.wholeAlbum)
}

const reviewBatchKey = (page: PageView): ReviewBatchKey => ({ batchId: page.batchId, artist: page.artist, album: page.album })

function pageOptions(review: ReviewState): ImportPageOptions {
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
  if (index < 0) return page
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
    album: candidate.trackAlbums[index] || '',
    inLibrary: candidate.inLibrary,
  }
}

function collectionSummary(page: PageView, selectedTrackUris: Iterable<string> = []) {
  let imported = 0
  let automatic = 0
  let suggested = 0
  let needsReview = 0
  for (const item of page.rows) {
    const track = matchedTrack(item)
    if (item.decision.status === 'done') imported += 1
    else if (track && !item.matchResult?.selectedUri && item.matchResult?.confidence === 'exact') automatic += 1
    else if (collectionSuggestion(item.source, item.matchResult, selectedTrackUris)) suggested += 1
    else if (!track) needsReview += 1
  }
  return { imported, automatic, suggested, needsReview }
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

function MatchPickerDialog({ kind, targetCount, query: initialQuery, candidates, selectedAlbums, selectedUri, selectedConfidence, busy, onCancel, onSearch, onChoose }: { kind: PickerKind; targetCount: number; query: string; candidates: AlbumCandidate[]; selectedAlbums: AlbumCandidate[]; selectedUri: string | null; selectedConfidence: MatchResult['confidence']; busy: boolean; onCancel: () => void; onSearch: (query: string) => void; onChoose: (uri: string) => void }) {
  const [query, setQuery] = useState(initialQuery)
  const [choice, setChoice] = useState(selectedUri ?? '')
  useEffect(() => { setQuery(initialQuery) }, [initialQuery])
  useEffect(() => { setChoice(selectedUri ?? '') }, [selectedUri])
  return <ModalDialog className="import-picker-dialog" labelledBy="import-picker-title" onCancel={onCancel} onSubmit={() => { if (choice) void onChoose(choice) }}>
    <header><p className="eyebrow">{kind === 'album' ? 'CHANGE ALBUM' : targetCount > 1 ? 'MAP SELECTED ROWS' : 'CHANGE TRACK'}</p><h2 id="import-picker-title">{kind === 'album' ? 'Choose a Spotify release' : targetCount > 1 ? `Choose one Spotify track for ${targetCount} Last.fm rows` : 'Choose a Spotify track'}</h2></header>
    {kind === 'track' && selectedAlbums.length > 0 && <label className="import-picker-album-tracks">Tracks from selected album matches<select autoFocus value={selectedAlbums.some((album) => album.trackUris.includes(choice)) ? choice : ''} disabled={busy} onChange={(event) => setChoice(event.target.value)}><option value="">Choose a track…</option>{selectedAlbums.map((album) => <optgroup key={album.uri} label={`${album.name} — ${album.artist}`}>{album.trackUris.map((uri, index) => <option key={`${album.uri}:${uri}`} value={uri}>{index + 1}. {album.trackNames[index] || `Track ${index + 1}`}</option>)}</optgroup>)}</select></label>}
    <div className="import-picker-search"><label htmlFor="import-picker-query">{kind === 'track' ? 'Search all Spotify' : 'Search Spotify or paste a share link'}</label><div><input id="import-picker-query" autoFocus={kind === 'album' || selectedAlbums.length === 0} value={query} onChange={(event) => setQuery(event.target.value)} onKeyDown={(event) => { if (event.key === 'Enter' && !busy && query.trim()) { event.preventDefault(); onSearch(query) } }} /><button type="button" disabled={busy || !query.trim()} onClick={() => onSearch(query)}>Search</button></div></div>
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
  const run = async <T extends PageView | AlbumCandidate[] | null>(request: () => Promise<T>) => {
    setLoading(true)
    try {
      const next = await request()
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
    void run(() => lastfmGateway.collectionSearchAlbums(page.batchId, page.artist, dialogState.query.trim())).then((next) => {
      if (next === null) setDialogState((state) => collectionDialogTransition(state, { type: 'search-failed' }))
    })
  }
  const openPreview = async (candidate: AlbumCandidate) => {
    setDialogState((state) => collectionDialogTransition(state, {
      type: 'preview-started',
      uri: candidate.uri,
      resultsScrollTop: resultList.current?.scrollTop ?? 0,
    }))
    const next = await run(() => lastfmGateway.collectionPreviewAlbum(page.batchId, page.artist, candidate.uri))
    setDialogState((state) => collectionDialogTransition(state, next ? { type: 'preview-succeeded', uri: candidate.uri } : { type: 'preview-failed' }))
  }
  const closePreview = (transition: 'back-to-results' | 'preview-add-succeeded' = 'back-to-results') => {
    setDialogState((state) => collectionDialogTransition(state, { type: transition }))
    requestAnimationFrame(() => { if (resultList.current) resultList.current.scrollTop = dialogState.resultsScrollTop })
  }
  const toggleMatch = async () => {
    if (!preview) return
    const selected = selectedUris.includes(preview.uri)
    const next = await run(() => selected
      ? lastfmGateway.collectionRemoveAlbum(page.batchId, page.artist, preview.uri)
      : lastfmGateway.collectionAddAlbum(page.batchId, page.artist, preview.uri))
    if (next && !selected) closePreview('preview-add-succeeded')
  }
  const toggleResultMatch = (candidate: AlbumCandidate) => {
    const selected = selectedUris.includes(candidate.uri)
    return run(() => selected
      ? lastfmGateway.collectionRemoveAlbum(page.batchId, page.artist, candidate.uri)
      : lastfmGateway.collectionAddAlbum(page.batchId, page.artist, candidate.uri))
  }
  const removeMatch = (uri: string) => void run(() => lastfmGateway.collectionRemoveAlbum(page.batchId, page.artist, uri))
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
      <p className="import-collection-attribution"><a href={`https://open.spotify.com/album/${preview.uri.split(':').pop()}`} onClick={(event) => { event.preventDefault(); void openExternalDestination({ kind: 'spotifyAlbum', id: preview.uri.split(':').pop() ?? '' }).catch(onError) }}>Open in Spotify ↗</a> · Track match state is shown above.</p>
      <footer><button type="button" onClick={() => closePreview()}>Back to results</button><button type="button" className="primary" disabled={busy || loading} onClick={() => void toggleMatch()}>{collectionAlbumActionLabel(selectedUris.includes(preview.uri))}</button></footer>
    </> : <>
      <div className="import-picker-search"><label htmlFor="collection-album-query">Search Spotify albums or paste a share link</label><div><input id="collection-album-query" autoFocus value={dialogState.query} onChange={(event) => setDialogState((state) => collectionDialogTransition(state, { type: 'set-query', query: event.target.value }))} /><button type="submit" disabled={busy || loading || !dialogState.query.trim()}>Search</button></div></div>
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
    <dl className="import-shortcuts-list"><dt>↑ / ↓</dt><dd>Move through the queue or mapping rows</dd><dt>Tab / Shift+Tab</dt><dd>Queue ↔ Last.fm source ↔ Spotify match</dd><dt>Enter</dt><dd>Open a queue batch or enter native controls</dd><dt>E</dt><dd>Open album matches or change a track match</dd><dt>Space</dt><dd>Toggle whole-album or track inclusion</dd><dt>X</dt><dd>Exclude or restore the focused source track</dd><dt>S</dt><dd>Skip or resume the focused album</dd><dt>A</dt><dd>Apply selections and advance</dd><dt>Esc</dt><dd>Exit control mode or close a dialog</dd><dt>?</dt><dd>Open this legend</dd></dl>
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

function ImporterRow({ item, rowNumber, checked, selected, needsMatch, collection, selectedTrackUris, ambiguousChoices, fuzzy, fuzzyTarget, fuzzyResultCount, onToggle, onExclude, onSelect, onChangeTrack, onUseTrack, onFuzzyMode, onFuzzyToggle, fuzzyExpanded, fuzzyMode, fuzzyLocked, showQuery, locked, rejectLocked }: { item: PageItem; rowNumber: number; checked: boolean; selected: boolean; needsMatch: boolean; collection: boolean; selectedTrackUris: Iterable<string>; ambiguousChoices: CollectionAmbiguousChoice[]; fuzzy?: ImportSourceRow[]; fuzzyTarget?: string; fuzzyResultCount: number; onToggle: () => void; onExclude: () => void; onSelect: (event: ReactMouseEvent<HTMLDivElement>) => void; onChangeTrack: () => void; onUseTrack: (uri: string) => void; onFuzzyMode: (mode: CountMode) => void; onFuzzyToggle: () => void; fuzzyExpanded: boolean; fuzzyMode: CountMode; fuzzyLocked: boolean; showQuery: boolean; locked: boolean; rejectLocked: boolean }) {
  const match = item.matchResult
  const track = matchedTrack(item)
  const displayedSearchTerm = collection && !track ? trackPickerQuery(item.source) : match?.searchTerm
  const suggestion = collection ? collectionSuggestion(item.source, item.matchResult, selectedTrackUris) : null
  const trackConfidence: ImportConfidence = selectedImportTrackConfidence(item.source.stableId, match?.selectedUri ?? null, match?.trackMatches ?? {}, match?.confidence ?? null, match?.candidates ?? [])
  const excluded = item.decision.excluded
  const imported = item.decision.status === 'done'
  const reviewable = ['pending', 'skipped'].includes(item.decision.status)
  const disabled = locked || excluded || !reviewable
  const excludeDisabled = rejectLocked || !reviewable
  return <article className={`import-track-row${excluded ? ' excluded' : ''}${selected ? ' selected' : ''}`} data-review-status={item.decision.status}>
    <div className="import-source-cell import-nav-target" data-import-nav="source" data-import-row={rowNumber} tabIndex={0} aria-label={`Last.fm source ${item.source.track}`} aria-keyshortcuts={IMPORT_NAV_KEYS} onClick={(event) => { if (!(event.target as Element).closest('button,input,select,textarea,a,label,[contenteditable="true"]')) onSelect(event) }}><button type="button" className="import-exclude-glyph" disabled={excludeDisabled} aria-label={excluded ? 'Undo exclusion' : `Exclude ${item.source.track}`} title={excluded ? 'Put this source row back in the queue' : 'Exclude this Last.fm source row'} onClick={onExclude}>{excluded ? '↺' : '⊘'}</button><label className="import-track-check"><input type="checkbox" aria-label={`Include ${item.source.track}`} checked={checked} disabled={disabled} onChange={onToggle} /><span /></label><div className="import-track-copy"><strong>{item.source.track}</strong><small>{item.source.playCount.toLocaleString()} plays · last {new Date(item.source.latest * 1000).toLocaleDateString()}</small>{imported && <small className="import-completed-copy">✓ Imported</small>}{excluded && <small className="import-excluded-copy">Excluded — won’t be imported or asked about again</small>}{fuzzy && fuzzyTarget && <FuzzyPanel targetTrack={track} resultCount={fuzzyResultCount} rows={fuzzy} targetUri={fuzzyTarget} mode={fuzzyMode} locked={fuzzyLocked || locked} expanded={fuzzyExpanded} onMode={onFuzzyMode} onToggle={onFuzzyToggle} />}</div></div>
    <div className={`import-match-cell import-nav-target${needsMatch ? ' needs-action' : ''}`} data-import-nav="match" data-import-row={rowNumber} tabIndex={0} aria-label={`Spotify match for ${item.source.track}`} aria-keyshortcuts={IMPORT_NAV_KEYS}>{track ? <><strong>{track.name}</strong><small>{track.artist}{track.album ? ` · ${track.album}` : ''}</small><span className={`confidence ${trackConfidence ?? 'low'}`}>{confidenceLabel(trackConfidence ?? 'low')}</span>{collection && trackConfidence === 'exact' && !imported && <span className="import-strong-match">STRONG MATCH</span>}{imported ? <span className="import-completed-badge">ALREADY IMPORTED</span> : collection && track.inLibrary && <span className="import-library-badge">ALREADY IN YOUR LIBRARY</span>}</> : ambiguousChoices.length ? <><strong className="import-action-required">Multiple matches</strong><small>Choose the Spotify track for this Last.fm row.</small><select className="import-ambiguity-select" aria-label={`Choose track match for ${item.source.track}`} value="" disabled={disabled} onChange={(event) => { if (event.target.value) onUseTrack(event.target.value) }}><option value="" disabled>Choose a track…</option>{ambiguousChoices.map((choice) => <option key={choice.uri} value={choice.uri}>{choice.track} — {choice.album}{choice.recommended ? ' — recommended' : ''}</option>)}</select></> : suggestion ? <><strong>{suggestion.name}</strong><small>{suggestion.artist} · {suggestion.trackAlbums[0] || 'Track result'}</small><span className="import-suggestion-label">SUGGESTED</span><button type="button" className="import-match-action" disabled={disabled} onClick={() => onUseTrack(suggestion.uri)}>Use This Track</button></> : needsMatch ? <><strong className="import-action-required">Action required</strong><small>No supported match</small></> : <small className="muted">No supported match</small>}{showQuery && displayedSearchTerm && <code>q={displayedSearchTerm}</code>}<button type="button" className="text-button" disabled={disabled} onClick={onChangeTrack}>Change Track…</button></div>
  </article>
}

function ImportGenreInput({ value, suggestions, disabled, onDraft, onCommit }: { value: string; suggestions: string[]; disabled: boolean; onDraft: (value: string) => void; onCommit: (value: string) => void }) {
  const [draft, setDraft] = useState(value)
  useEffect(() => {
    setDraft(value)
    onDraft(value)
  }, [onDraft, value])
  return <AutocompleteInput ariaLabel="Import genre" disabled={disabled} suggestions={suggestions} value={draft} onValue={(next) => {
    setDraft(next)
    onDraft(next)
  }} onBlur={() => onCommit(draft)} onKeyDown={(event) => {
    if (event.key === 'Enter' && !event.nativeEvent.isComposing) {
      event.preventDefault()
      event.currentTarget.blur()
    }
  }} placeholder="No change" />
}

function ImportPage({ page, failed, showQueries, onRefresh, onRejected, onNext, onApplied, onPrevious, onError, onCollectionPage, onTabToQueue, onShortcuts, onStatus, onMutation }: { page: PageView; failed: boolean; showQueries: boolean; onRefresh: (strict?: boolean) => Promise<ImportQueueItem[]>; onRejected: (page: PageView, remainingPlayCount: number, allExcluded: boolean) => ImportQueueItem[]; onNext: (queue?: ImportQueueItem[], focusQueue?: boolean) => void; onApplied: () => Promise<void>; onPrevious: () => void; onError: (error: unknown) => void; onCollectionPage: (page: PageView) => void; onTabToQueue: () => boolean; onShortcuts: () => void; onStatus: ShortcutStatus; onMutation: (running: boolean) => void }) {
  const [review, setReview] = useState<ReviewState>(() => reviewForPage(page))
  const genreDraft = useRef(review.genre)
  const updateGenreDraft = useCallback((genre: string) => { genreDraft.current = genre }, [])
  const [busy, setBusy] = useState(false)
  const [savingRejects, setSavingRejects] = useState(false)
  const [applyState, setApplyState] = useState<'ready' | 'enqueueing' | 'loading' | 'error'>('ready')
  const [picker, setPicker] = useState<PickerState | null>(null)
  const [collectionDialogOpen, setCollectionDialogOpen] = useState(false)
  const [collectionPreviewUri, setCollectionPreviewUri] = useState<string>()
  const [selectedAlbumsExpanded, setSelectedAlbumsExpanded] = useState(true)
  const [genreSuggestions, setGenreSuggestions] = useState<string[]>([])
  const selectedAlbums = useRef<HTMLDetailsElement>(null)
  const [expandedFuzzy, setExpandedFuzzy] = useState<Set<string>>(new Set())
  const [pageError, setPageError] = useState<string>()
  const [rowSelection, setRowSelection] = useState<ImportRowSelection>({ ids: new Set(), anchor: null })
  const queuedRejects = useRef({ exclude: new Set<string>(), restore: new Set<string>() })
  const rejectSaveRunning = useRef(false)
  const latestRejectProjection = useRef<{ page: PageView; remainingPlayCount: number; allExcluded: boolean } | undefined>(undefined)
  const reviewKey = useRef(reviewBatchKey(page))
  useEffect(() => {
    const nextKey = reviewBatchKey(page)
    setReview((current) => mergeReviewBatchDraft(current, reviewKey.current, reviewForPage(page), nextKey))
    const changed = reviewKey.current.batchId !== nextKey.batchId || reviewKey.current.artist !== nextKey.artist || reviewKey.current.album !== nextKey.album
    reviewKey.current = nextKey
    if (changed) { setExpandedFuzzy(new Set()); setPageError(undefined) }
    if (!page.collection) { setCollectionDialogOpen(false); setCollectionPreviewUri(undefined) }
  }, [page])
  useEffect(() => { setRowSelection({ ids: new Set(), anchor: null }) }, [page.batchId])
  useEffect(() => {
    let active = true
    libraryGateway.genreValues().then((values) => { if (active) setGenreSuggestions(values) }).catch((error) => { if (active) onError(error) })
    return () => { active = false }
  }, [onError])
  const withGenreDraft = (next: ReviewState): ReviewState => next.genre === genreDraft.current ? next : { ...next, genre: genreDraft.current }
  const saveOptions = (next: ReviewState) => lastfmGateway.saveOptions(page, pageOptions(withGenreDraft(next)))
  const persist = async (next: ReviewState, refreshQueue = false) => {
    next = withGenreDraft(next)
    setReview(next)
    setBusy(true)
    onMutation(true)
    try {
      await saveOptions(next)
      if (refreshQueue) await onRefresh()
    } catch (error) { onError(error) } finally { onMutation(false); setBusy(false) }
  }
  const persistGenre = (genre: string) => {
    const next = { ...review, genre }
    if (genre !== review.genre) setReview(next)
    void saveOptions(next).catch(onError)
  }
  const run = async (request: () => Promise<unknown>, onSuccess: (queue: ImportQueueItem[]) => void = () => {}): Promise<boolean> => {
    setBusy(true)
    onMutation(true)
    try {
      return await runCheckedImportMutation(request, () => onRefresh(true), onSuccess, onError)
    } finally { onMutation(false); setBusy(false) }
  }
  const runPageMutation = async (request: () => Promise<PageView | null>): Promise<PageView | null> => {
    setBusy(true)
    onMutation(true)
    try {
      const next = await request()
      if (next) onCollectionPage(next)
      return next
    } catch (error) { onError(error); return null } finally { onMutation(false); setBusy(false) }
  }
  const flushRejectedRows = async () => {
    let nextState: ImportStateView | undefined
    try {
      while (queuedRejects.current.exclude.size || queuedRejects.current.restore.size) {
        const action = queuedRejects.current.exclude.size ? 'exclude' : 'undo-exclude'
        const pending = action === 'exclude' ? queuedRejects.current.exclude : queuedRejects.current.restore
        const ids = [...pending]
        pending.clear()
        nextState = await lastfmGateway.review({ batchId: page.batchId, ids, action, artist: page.artist, album: page.album })
      }
      const projection = latestRejectProjection.current
      if (nextState && projection) {
        const nextPage = { ...projection.page, state: nextState }
        const nextQueue = onRejected(nextPage, projection.remainingPlayCount, projection.allExcluded)
        if (!projection.remainingPlayCount) onNext(nextQueue)
      }
    } catch (error) {
      queuedRejects.current.exclude.clear()
      queuedRejects.current.restore.clear()
      try { await onRefresh(true) } catch { /* Preserve the mutation error. */ }
      setPageError(String(error))
      onError(error)
    } finally {
      rejectSaveRunning.current = false
      setSavingRejects(false)
      setBusy(false)
      onMutation(false)
    }
  }
  const rejectRows = (ids: Iterable<string>, excluded: boolean) => {
    const rowIds = [...new Set(ids)]
    if (!rowIds.length || (busy && !rejectSaveRunning.current)) return
    const nextReview = excludeImportRows(review, rowIds, excluded)
    const nextDecisions = page.rows.map((item) => nextReview.decisions[item.source.stableId] ?? item.decision)
    const remainingPlayCount = page.rows.reduce((total, item, index) => ['pending', 'skipped'].includes(nextDecisions[index].status) && !nextDecisions[index].excluded ? total + item.source.playCount : total, 0)
    const allExcluded = nextDecisions.every((decision) => decision.excluded)
    flushSync(() => {
      setReview(nextReview)
      setRowSelection((current) => ({ ids: new Set([...current.ids].filter((id) => !rowIds.includes(id))), anchor: rowIds.includes(current.anchor ?? '') ? null : current.anchor }))
      setPageError(undefined)
    })
    const nextPage = { ...page, rows: page.rows.map((item, index) => ({ ...item, decision: nextDecisions[index] })) }
    onRejected(nextPage, remainingPlayCount, allExcluded)
    latestRejectProjection.current = { page: nextPage, remainingPlayCount, allExcluded }
    for (const id of rowIds) {
      if (excluded) {
        if (!queuedRejects.current.restore.delete(id)) queuedRejects.current.exclude.add(id)
      } else if (!queuedRejects.current.exclude.delete(id)) {
        queuedRejects.current.restore.add(id)
      }
    }
    if (!rejectSaveRunning.current) {
      rejectSaveRunning.current = true
      setSavingRejects(true)
      setBusy(true)
      onMutation(true)
      requestAnimationFrame(() => setTimeout(() => void flushRejectedRows()))
    }
  }
  const projectedRows = page.rows.map((item) => ({ ...item, decision: review.decisions[item.source.stableId] ?? item.decision }))
  const projectedPage = { ...page, rows: projectedRows }
  const reviewableAlbumRows = projectedRows.filter((item) => !item.decision.excluded && (item.decision.status === 'pending' || item.decision.status === 'skipped'))
  const canResumeAlbum = reviewableAlbumRows.length > 0 && reviewableAlbumRows.every((item) => item.decision.status === 'skipped')
  const toggleAlbumSkip = async () => {
    const action = canResumeAlbum ? 'restore' : 'skip-album'
    await run(() => lastfmGateway.review({ batchId: page.batchId, action, artist: page.artist, album: page.album }), (nextQueue) => {
      onStatus(action === 'restore' ? `${page.customBatch ? 'Batch' : 'Album'} resumed.` : `${page.customBatch ? 'Batch' : 'Album'} skipped. Press S again to resume it.`)
      if (importAlbumActionAdvances(action)) onNext(nextQueue)
    })
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
      const currentReview = withGenreDraft(review)
      setReview(currentReview)
      setPageError(undefined)
      setApplyState('enqueueing')
      await lastfmGateway.apply(page, [...currentReview.checked], advance, pageOptions(currentReview))
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
      await lastfmGateway.retryApply(page.batchId)
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
  const pickerCandidatePool = picker ? [...new Map(picker.sourceIds.flatMap((sourceId) => page.rows.find((item) => item.source.stableId === sourceId)?.matchResult?.candidates ?? []).map((candidate) => [candidate.uri, candidate])).values()] : []
  const openTrackPicker = (sourceId: string) => {
    const item = page.rows.find((entry) => entry.source.stableId === sourceId)
    setPicker({ kind: 'track', sourceId, sourceIds: [sourceId], query: item ? trackPickerQuery(item.source) : '' })
  }
  const openAlbumPicker = () => {
    const sourceId = page.rows[0]?.source.stableId ?? ''
    setPicker({ kind: 'album', sourceId, sourceIds: [sourceId], query: page.album })
  }
  const openCollectionAlbums = (uri?: string) => { setCollectionPreviewUri(uri); setCollectionDialogOpen(true) }
  const activateCollection = async () => {
    const next = await runPageMutation(() => lastfmGateway.activateCollection(page))
    if (next) openCollectionAlbums()
  }
  const removeCollectionAlbum = async (uri: string) => {
    setBusy(true)
    onMutation(true)
    try {
      const next = await lastfmGateway.collectionRemoveAlbum(page.batchId, page.artist, uri)
      if (next) onCollectionPage(next)
    } catch (error) { onError(error) } finally { onMutation(false); setBusy(false) }
  }
  const searchPicker = async (query: string) => {
    if (!picker) return
    const activePicker = picker
    if (activePicker.kind === 'track') await runPageMutation(() => lastfmGateway.changeTrack(page.batchId, activePicker.sourceId, query))
    else await run(() => lastfmGateway.changeAlbum(page.batchId, activePicker.sourceId, query))
    setPicker((current) => current && current.kind === activePicker.kind && current.sourceId === activePicker.sourceId ? { ...current, query } : current)
  }
  const choosePicker = async (uri: string) => {
    if (!picker) return
    const activePicker = picker
    const next = await runPageMutation(() => activePicker.kind === 'track'
      ? lastfmGateway.selectMatches(page.batchId, activePicker.sourceIds.map((id) => ({ id, uri })))
      : lastfmGateway.selectMatch(page.batchId, activePicker.sourceId, uri))
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
      onFuzzyMode: (nextMode: CountMode) => void run(() => lastfmGateway.countMode(group.target, nextMode)),
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
      if (row === 0) {
        if (collection) openCollectionAlbums()
        else openAlbumPicker()
      }
      else if (item && !item.decision.excluded && ['pending', 'skipped'].includes(item.decision.status)) openTrackPicker(item.source.stableId)
      else onStatus('This source track is already done or excluded.')
      return
    }
    if (event.key === ' ') {
      if (row === 0) {
        if (collection) onStatus('Use Import full album on each match to choose full albums.')
        else if (!review.importContent) onStatus('Enable content import before selecting the whole album.')
        else void persist(setWholeAlbumImport(review, !review.wholeAlbum), true)
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
      else void rejectRows([item.source.stableId], !item.decision.excluded)
      return
    }
    if (event.key.toLowerCase() === 's') {
      if (!reviewableAlbumRows.length) onStatus('This album is done, ignored, or excluded; nothing changed.')
      else void toggleAlbumSkip()
      return
    }
    if (event.key.toLowerCase() === 'a') void apply(true)
  }
  const selectedAlbumItem = projectedRows.find((item) => item.matchResult?.selectedUri?.startsWith('spotify:album:'))
  const selectedAlbumUri = selectedAlbumItem?.matchResult?.selectedUri
  const selectedAlbumCandidate = selectedAlbumItem?.matchResult?.candidates.find((candidate) => candidate.uri === selectedAlbumUri)
  const albumAdvisory = selectedAlbumAdvisory(projectedPage)
  const selectedAlbumName = selectedAlbumCandidate?.name ?? 'Choose a release'
  const selectedActionableIds = projectedRows.filter((item) => review.checked.has(item.source.stableId) && !item.decision.excluded && (item.decision.status === 'pending' || item.decision.status === 'skipped')).map((item) => item.source.stableId)
  const matchedIds = projectedRows.filter((item) => matchedTrack(item)).map((item) => item.source.stableId)
  const requiredMatchIds = requiredImportMatchIds(selectedActionableIds, matchedIds, review.includeHistoricalPlayCounts, review.wholeAlbum)
  const requiredMatches = new Set(requiredMatchIds)
  const collection = page.collection !== null
  const collectionMatches = page.collection
  const selectedCollectionUris = collectionMatches ? selectedCollectionAlbumUris(collectionMatches.cachedAlbums, collectionMatches.selectedAlbumUris) : []
  const addedCollectionAlbums = collectionMatches?.cachedAlbums.filter((candidate) => selectedCollectionUris.includes(candidate.uri)) ?? []
  const selectedCollectionAlbumSummary = `${selectedCollectionUris.length} ${selectedCollectionUris.length === 1 ? 'album' : 'albums'} added`
  const collectionAlbumReady = !collection || Boolean(selectedAlbumUri && albumAdvisory.strong)
  const wholeAlbumDisabled = busy || failed || !review.importContent || !collectionAlbumReady
  const selectedCollectionTrackUris = collectionMatches
    ? new Set(collectionMatches.cachedAlbums.filter((candidate) => selectedCollectionUris.includes(candidate.uri)).flatMap((candidate) => candidate.trackUris))
    : new Set<string>()
  const rejectedIds = projectedRows.filter((item) => item.decision.excluded).map((item) => item.source.stableId)
  const visibleRows = stablePartitionImportRows(projectedRows, requiredMatchIds, (item) => item.source.stableId, rejectedIds)
  const rejectableIds = visibleRows.filter((item) => !item.decision.excluded && ['pending', 'skipped'].includes(item.decision.status)).map((item) => item.source.stableId)
  const selectedRowIds = [...rowSelection.ids].filter((id) => rejectableIds.includes(id))
  const selectRejectRow = (id: string, event: ReactMouseEvent<HTMLDivElement>) => {
    if (busy || failed) return
    setRowSelection(selectImportRows(rejectableIds, rowSelection.ids, rowSelection.anchor, id, event))
  }
  const selectAllRows = () => setRowSelection({ ids: new Set(rejectableIds), anchor: rejectableIds[0] ?? null })
  const mapSelectedRows = () => {
    const sourceId = selectedRowIds[0]
    const item = page.rows.find((entry) => entry.source.stableId === sourceId)
    if (sourceId && item) setPicker({ kind: 'track', sourceId, sourceIds: selectedRowIds, query: trackPickerQuery(item.source) })
  }
  const summary = collection ? collectionSummary(projectedPage, selectedCollectionTrackUris) : null
  const suggestedMatches = collection ? visibleRows.flatMap((item) => {
    if (!review.checked.has(item.source.stableId) || item.decision.excluded || !['pending', 'skipped'].includes(item.decision.status)) return []
    const suggestion = collectionSuggestion(item.source, item.matchResult, selectedCollectionTrackUris)
    return suggestion ? [{ id: item.source.stableId, uri: suggestion.uri }] : []
  }) : []
  const applySuggestedMatches = async () => {
    const next = await runPageMutation(() => lastfmGateway.selectMatches(page.batchId, suggestedMatches))
    if (next) onStatus(`${suggestedMatches.length} suggested ${suggestedMatches.length === 1 ? 'match' : 'matches'} selected.`)
  }
  // Release-shaped batches can opt into the same collection workflow used by
  // batches without Last.fm album metadata.
  return <section className="import-review" aria-labelledby="import-review-title" aria-busy={applyState === 'enqueueing' || applyState === 'loading'} onKeyDown={handleNavigationKeyDown}>
    <header className="import-review-header"><div><p className="eyebrow">{page.artist}</p><h2 id="import-review-title">{page.customBatch ? 'Custom batch' : page.album || 'Singles'}</h2><p className="import-page-meta">{page.rows.length} source tracks · {page.rows.reduce((total, item) => total + item.source.playCount, 0).toLocaleString()} plays</p>{collection && <><p className="import-collection-note">{page.customBatch ? 'You combined these Last.fm batches. Added Spotify albums constrain and rerank all of their track choices together.' : page.album ? 'This Last.fm release started with one Spotify match. Added Spotify albums can narrow and rerank its track choices.' : 'Last.fm supplied no album metadata. Tracks are matched individually. Added Spotify albums can narrow the choices.'}</p>{summary && <div className="import-collection-summary">{summary.imported > 0 && <span className="is-imported">{summary.imported} tracks imported</span>}<span className="is-automatic">{summary.automatic} automatically selected</span><span className="is-suggested">{summary.suggested} suggested</span><span className="needs-review">{summary.needsReview} need review</span></div>}</>}</div><div className="import-page-actions"><button type="button" disabled={busy || failed || page.pageNumber <= 1} aria-label="Previous batch" onClick={onPrevious}>‹</button><span>Batch {page.pageNumber} of {page.pageCount}</span><button type="button" disabled={busy || failed || page.pageNumber >= page.pageCount} aria-label="Next batch" onClick={() => onNext()}>›</button>{collection ? <button type="button" disabled={busy || failed} onClick={() => openCollectionAlbums()}>Manage Albums…</button> : <><button type="button" disabled={busy || failed} onClick={openAlbumPicker}>Change Album…</button><button type="button" disabled={busy || failed} onClick={() => void activateCollection()}>Add Album…</button></>}<button type="button" disabled={busy || failed} onClick={() => void toggleAlbumSkip()}>{canResumeAlbum ? `Resume ${page.customBatch ? 'Batch' : 'Album'}` : `Skip ${page.customBatch ? 'Batch' : 'Album'}`}</button>{!page.customBatch && <><button type="button" disabled={busy || failed} onClick={() => void run(() => lastfmGateway.review({ batchId: page.batchId, action: 'ignore-album', artist: page.artist, album: page.album }), onNext)}>Ignore Album</button><button type="button" disabled={busy || failed} onClick={() => void run(() => lastfmGateway.review({ batchId: page.batchId, action: 'ignore-artist', artist: page.artist, album: page.album }), onNext)}>Ignore Artist</button></>}</div></header>
    <div className="import-album-strip"><div className="import-nav-target" data-import-nav="source" data-import-row="0" tabIndex={0} aria-label="Last.fm album source" aria-keyshortcuts={IMPORT_NAV_KEYS}><p>{collection && !page.album && !page.customBatch ? 'WHAT LAST.FM SUPPLIED' : 'WHAT I’M IMPORTING'}</p><strong>{page.customBatch ? 'Custom batch' : page.album || 'Singles'}</strong><small>{page.customBatch ? `${page.artist} · ${page.albumLabelCount ?? 0} source albums · ${page.rows.length} source tracks` : collection && !page.album ? `${page.artist} · no album metadata · ${page.rows.length} source tracks matched individually` : `${page.artist} · ${page.rows.length} source tracks${collection ? ' · matched across Spotify albums' : ''}`}</small></div>{collection ? <div className="import-nav-target" data-import-nav="match" data-import-row="0" tabIndex={0} aria-label="Spotify album matches" aria-keyshortcuts={IMPORT_NAV_KEYS}><p>SPOTIFY ALBUM MATCHES</p><strong>{selectedCollectionUris.length ? selectedCollectionAlbumSummary : 'No album matches yet'}</strong><small>{collectionMatches ? collectionCoverageStatus(collectionMatches.coverage) : 'Search Spotify to build a match set'}</small><button type="button" disabled={busy || failed} onClick={() => openCollectionAlbums()}>{page.album ? 'Manage Albums…' : 'Add albums…'}</button></div> : <div className="import-nav-target" data-import-nav="match" data-import-row="0" tabIndex={0} aria-label="Spotify album match" aria-keyshortcuts={IMPORT_NAV_KEYS}><p>SPOTIFY MATCH</p><strong>{selectedAlbumName}</strong><small>{selectedAlbumCandidate ? relationLabel(selectedAlbumCandidate.relation) : 'No release selected'}</small>{albumAdvisory.strong && <span className="import-strong-match" role="status">STRONG MATCH{albumAdvisory.extraTrackCount ? ` · ${albumAdvisory.extraTrackCount} extra Spotify track${albumAdvisory.extraTrackCount === 1 ? '' : 's'}` : ''}</span>}<button type="button" disabled={busy || failed} onClick={openAlbumPicker}>Change Album…</button></div>}</div>
    {collection && collectionMatches && selectedCollectionUris.length > 0 && <><details ref={selectedAlbums} className="import-selected-album-cards" open={selectedAlbumsExpanded} onToggle={(event) => setSelectedAlbumsExpanded(event.currentTarget.open)}><summary><strong>{selectedCollectionAlbumSummary}</strong><small>{collectionCoverageStatus(collectionMatches.coverage)}</small></summary>{selectedCollectionUris.map((uri) => { const candidate = collectionMatches.cachedAlbums.find((entry) => entry.uri === uri); const coverage = collectionMatches.coverage.selectedAlbums.find((entry) => entry.uri === uri); if (!candidate) return null; const metadata = [candidate.artist, candidate.releaseDate?.slice(0, 4), candidate.albumType].filter(Boolean).join(' · '); const importAlbum = collectionMatches.fullAlbumUris.includes(uri); return <article className="import-selected-album-card" key={uri}><div className="import-selected-album-art">{candidate.imageUrl ? <img src={candidate.imageUrl} alt="" /> : <span aria-hidden="true">♪</span>}</div><div className="import-selected-album-copy"><strong>{candidate.name}</strong><small>{metadata} · {coverage?.matched ?? 0} matches · {coverage?.uniqueCoverage ?? 0} unique</small></div><span className="import-album-source">MATCH SET</span><label className="import-album-import-option"><input type="checkbox" aria-label={`Import full album: ${candidate.name}`} checked={importAlbum} disabled={busy || failed || !review.importContent} onChange={(event) => { const enabled = event.currentTarget.checked; void runPageMutation(() => lastfmGateway.collectionSetAlbumImport(page.batchId, page.artist, uri, enabled)) }} /><span><span>Import full album</span><small>{importAlbum ? 'Full album' : 'Matched tracks only'}</small></span></label><button type="button" disabled={busy || failed} onClick={() => openCollectionAlbums(uri)}>Preview</button><button type="button" disabled={busy || failed} onClick={() => void removeCollectionAlbum(uri)}>Remove</button></article> })}</details>{selectedAlbumsExpanded && <VerticalResizeHandle target={selectedAlbums} label="Resize album matches" minHeight={58} />}</>}
    <div className="import-options" role="group" aria-label="Import options"><label><input type="checkbox" aria-label="Import tracks and albums found in history" checked={review.importContent} disabled={busy || failed || (!review.includeHistoricalPlayCounts && review.importContent)} onChange={(event) => intentChange('importContent', event.target.checked)} /> Import tracks and albums found in history</label><label><input type="checkbox" aria-label="Include historical play counts" checked={review.includeHistoricalPlayCounts} disabled={busy || failed || (!review.importContent && review.includeHistoricalPlayCounts)} onChange={(event) => intentChange('includeHistoricalPlayCounts', event.target.checked)} /> Include historical play counts</label>{!collection && <label><input type="checkbox" aria-label="Import whole album" checked={review.wholeAlbum} disabled={wholeAlbumDisabled} onChange={(event) => void persist(setWholeAlbumImport(review, event.target.checked), true)} /> Import whole album</label>}<label>Genre <ImportGenreInput key={`${page.batchId}:${page.artist}:${page.album}`} value={review.genre} suggestions={genreSuggestions} disabled={busy || failed} onDraft={updateGenreDraft} onCommit={persistGenre} /></label><label>Rating <select aria-label="Import rating" disabled={busy || failed} value={review.rating ?? ''} onChange={(event) => void persist({ ...review, rating: event.target.value ? Number(event.target.value) : null })}><option value="">No change</option>{[1, 2, 3, 4, 5].map((rating) => <option key={rating} value={rating}>{'★'.repeat(rating)}</option>)}</select></label></div>
    {(review.wholeAlbum || Boolean(collectionMatches?.fullAlbumUris.length)) && <p className="import-exclusion-note">Exclude removes only this Last.fm source row. A track inherently included by a full album cannot be removed from Spotify here.</p>}
    <div className="import-track-actions"><button type="button" disabled={busy || failed || !rejectableIds.length || selectedRowIds.length === rejectableIds.length} onClick={selectAllRows}>Select all rows</button><button type="button" disabled={busy || failed || !selectedRowIds.length} onClick={() => setRowSelection({ ids: new Set(), anchor: null })}>Clear selection</button><button type="button" disabled={busy || failed || selectedRowIds.length < 2} onClick={mapSelectedRows}>Map selected ({selectedRowIds.length})…</button><span>Click or Shift-click source rows to select them.</span></div>
    <div className="import-track-list">{visibleRows.map((item, index) => { const itemFuzzy = fuzzy(item); const ambiguousChoices = collectionMatches ? collectionAmbiguousChoices(item.source.stableId, item.matchResult, collectionMatches.cachedAlbums, selectedCollectionUris, collectionMatches.coverage.selectedAlbums) : []; return <ImporterRow key={item.source.stableId} item={item} rowNumber={index + 1} checked={review.checked.has(item.source.stableId)} selected={rowSelection.ids.has(item.source.stableId)} needsMatch={requiredMatches.has(item.source.stableId)} collection={collection} selectedTrackUris={selectedCollectionTrackUris} ambiguousChoices={ambiguousChoices} showQuery={showQueries} onToggle={() => void persist(toggleImportRow(review, item.source.stableId), true)} onExclude={() => void rejectRows([item.source.stableId], !item.decision.excluded)} onSelect={(event) => selectRejectRow(item.source.stableId, event)} onChangeTrack={() => openTrackPicker(item.source.stableId)} onUseTrack={(uri) => void runPageMutation(() => lastfmGateway.selectMatch(page.batchId, item.source.stableId, uri))} {...itemFuzzy} fuzzyExpanded={itemFuzzy.fuzzyExpanded ?? false} fuzzyMode={itemFuzzy.fuzzyMode ?? 'sum'} fuzzyLocked={itemFuzzy.fuzzyLocked ?? false} onFuzzyMode={itemFuzzy.onFuzzyMode ?? (() => {})} onFuzzyToggle={itemFuzzy.onFuzzyToggle ?? (() => {})} locked={failed || busy} rejectLocked={failed || (busy && !savingRejects)} /> })}</div>
    {pageError && <p className="import-page-error" role="alert">{pageError}</p>}
    <footer className="import-review-footer"><span>{failed ? 'This batch failed and its choices are frozen.' : `${selectedImportCount(review)} selected · ${excludedImportCount(review)} excluded · ${restPendingImportCount(review)} not selected`}</span><div>{failed ? <button type="button" className="primary" disabled={busy} onClick={() => void retry()}>Retry Apply</button> : <><button type="button" disabled={busy || !selectedRowIds.length} onClick={() => void rejectRows(selectedRowIds, true)}>Reject selected ({selectedRowIds.length})</button>{suggestedMatches.length > 0 && <button type="button" disabled={busy} onClick={() => void applySuggestedMatches()}>Use {suggestedMatches.length} Suggestions</button>}<button type="button" disabled={busy || !selectedImportCount(review)} onClick={() => void apply(false)}>Accept Changes</button><button type="button" className="primary" disabled={busy || !selectedImportCount(review)} onClick={() => void apply(true)}>Accept &amp; Next Batch</button></>}</div></footer>
    {collectionDialogOpen && collectionMatches && <CollectionAlbumDialog page={page} collection={collectionMatches} initialPreviewUri={collectionPreviewUri} busy={busy || failed} onCancel={() => { setCollectionDialogOpen(false); setCollectionPreviewUri(undefined) }} onPage={onCollectionPage} onError={onError} />}
    {picker && pickerItem && <MatchPickerDialog kind={picker.kind} targetCount={picker.sourceIds.length} query={picker.query} candidates={pickerCandidates(picker.kind, pickerCandidatePool)} selectedAlbums={collection ? addedCollectionAlbums : selectedAlbumCandidate ? [selectedAlbumCandidate] : []} selectedUri={pickerSelectedUri(picker.kind, picker.sourceId, pickerMatch?.selectedUri ?? null, pickerMatch?.trackMatches ?? {})} selectedConfidence={pickerMatch?.confidence ?? null} busy={busy || failed} onCancel={() => setPicker(null)} onSearch={searchPicker} onChoose={choosePicker} />}
  </section>
}

const IMPORT_QUEUE_ROW_HEIGHT = 57
const IMPORT_QUEUE_OVERSCAN = 4

function importQueueItemTitle(item: ImportQueueItem): string {
  return item.customBatch ? 'Custom batch' : item.album || 'Singles'
}

function VirtualQueue({ items, selectedPage, selectedBatchPages, disabled, onOpen, onSelect, onSkip, onTab, onShortcuts }: { items: ImportQueueItem[]; selectedPage: number | null; selectedBatchPages: Set<number>; disabled: boolean; onOpen: (item: ImportQueueItem) => void; onSelect: (page: number, selected: boolean) => void; onSkip: (item: ImportQueueItem) => void; onTab: (shiftKey: boolean) => ImportQueueTabTarget | null; onShortcuts: () => void }) {
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
        {items.slice(range.start, range.end).map((item, index) => {
          const absoluteIndex = range.start + index
          const title = importQueueItemTitle(item)
          return <div className="import-queue-entry" key={item.page}>
            <label className="import-queue-select"><input type="checkbox" aria-label={`Select ${title} by ${item.artist}`} checked={selectedBatchPages.has(item.page)} disabled={disabled || item.status === 'failed'} onChange={(event) => onSelect(item.page, event.currentTarget.checked)} /></label>
            <button type="button" data-import-nav="queue" data-queue-index={absoluteIndex} data-highlighted={highlightedIndex === absoluteIndex ? 'true' : undefined} aria-current={selectedPage === item.page ? 'true' : undefined} aria-keyshortcuts="ArrowUp ArrowDown Tab Shift+Tab Enter S ?" tabIndex={highlightedIndex === absoluteIndex ? 0 : -1} aria-label={`Batch ${absoluteIndex + 1} of ${items.length}: ${title} by ${item.artist}, ${item.remainingPlayCount.toLocaleString()} plays to import${item.importedPlayCount ? `, ${item.importedPlayCount.toLocaleString()} plays already imported` : ''}`} disabled={disabled} className={`import-queue-row${selectedPage === item.page ? ' selected' : ''}${highlightedIndex === absoluteIndex ? ' highlighted' : ''}`} onFocus={() => { highlightedPageRef.current = item.page; setHighlightedIndex((current) => current === absoluteIndex ? current : absoluteIndex) }} onKeyDown={(event) => handleKeyDown(event, item, absoluteIndex)} onClick={() => { highlightedPageRef.current = item.page; setHighlightedIndex(absoluteIndex); onOpen(item) }}><span className={`import-status-dot ${item.status ?? 'pending'}`} aria-label={item.status ?? 'pending'} title={item.error ?? undefined}>{item.status === 'done' ? '✓' : item.status === 'skipped' ? '–' : item.status === 'failed' ? '!' : item.status === 'excluded' || item.status?.startsWith('ignored') ? '⊘' : '•'}</span><span className="import-queue-copy"><strong>{title}</strong><small>{item.error ? `Apply failed: ${item.error}` : `${item.artist} · ${item.sourceCount} tracks`}</small></span><span className="import-queue-count"><span className="plays-to-import">{item.remainingPlayCount.toLocaleString()} <small>plays to import</small></span>{item.importedPlayCount > 0 && <span className="plays-imported">{item.importedPlayCount.toLocaleString()} <small>plays imported</small></span>}</span></button>
          </div>
        })}
      </div>
    </div>
  </div>
}

function ImportQueueFilter({ value, onValue }: { value: string; onValue: (value: string) => void }) {
  const [draft, setDraft] = useState(value)
  const timer = useRef(0)
  useEffect(() => {
    window.clearTimeout(timer.current)
    setDraft(value)
  }, [value])
  useEffect(() => () => window.clearTimeout(timer.current), [])
  const commit = (next: string) => {
    window.clearTimeout(timer.current)
    if (next !== value) onValue(next)
  }
  return <input type="search" aria-label="Filter import queue" placeholder="Filter" value={draft} onChange={(event) => {
    const next = event.currentTarget.value
    setDraft(next)
    window.clearTimeout(timer.current)
    timer.current = window.setTimeout(() => onValue(next), 100)
  }} onBlur={() => commit(draft)} onKeyDown={(event) => {
    if (event.key === 'Enter' && !event.nativeEvent.isComposing) {
      event.preventDefault()
      commit(draft)
    }
  }} />
}

export default function LastFmImporter() {
  const [state, setState] = useState(emptyState)
  const [queue, setQueue] = useState<ImportQueueItem[]>([])
  const [queueFilter, setQueueFilter] = useState('')
  const [selectedBatchPages, setSelectedBatchPages] = useState<Set<number>>(() => new Set())
  const [sort, setSort] = useState<'plays' | 'artist' | 'batch' | 'lastPlayed'>('plays')
  const [showQueries, setShowQueries] = useState(true)
  const [selected, setSelected] = useState<ImportQueueItem | null>(null)
  const [page, setPage] = useState<PageView | null>(null)
  const [busy, setBusy] = useState(false)
  const [pageLoading, setPageLoading] = useState(false)
  const [error, setError] = useState<DisplayError | null>(null)
  const [acceptAllOpen, setAcceptAllOpen] = useState(false)
  const [acceptAllSummary, setAcceptAllSummary] = useState<{ albumEntities: number; trackEntities: number } | null>(null)
  const [shortcutsOpen, setShortcutsOpen] = useState(false)
  const [shortcutStatus, setShortcutStatus] = useState('')
  const [pendingDefaults, setPendingDefaults] = useState<LastFmImportDefaults>(emptyDefaults)
  const [pageMutationRunning, setPageMutationRunning] = useState(false)
  const reportError = useCallback((reason: unknown) => setError({ message: String(reason), code: 'apply-failed', retryAt: null }), [])
  const refreshGeneration = useRef(0)
  const pageRequestGeneration = useRef(0)
  const acceptAllRunning = useRef(false)
  const queueMutationRunning = useRef(false)
  const focusQueueAfterOpen = useRef(false)
  const advancingApply = useRef(false)
  const prefetchedTransition = useRef('')
  const orderedQueue = useMemo(() => sortImportQueue(queue, sort), [queue, sort])
  const activeQueue = useMemo(() => activeImportQueue(orderedQueue), [orderedQueue])
  const filteredQueue = useMemo(() => filterImportQueue(activeQueue, queueFilter), [activeQueue, queueFilter])
  const selectableFilteredQueue = filteredQueue.filter((item) => item.status !== 'failed')
  const selectAllBatches = useRef<HTMLInputElement>(null)
  const selectedBatchIds = activeQueue.filter((item) => item.status !== 'failed' && selectedBatchPages.has(item.page)).map((item) => item.page)
  const selectedFilteredCount = selectableFilteredQueue.filter((item) => selectedBatchPages.has(item.page)).length
  const allFilteredSelected = selectableFilteredQueue.length > 0 && selectedFilteredCount === selectableFilteredQueue.length
  useEffect(() => {
    if (selectAllBatches.current) selectAllBatches.current.indeterminate = selectedFilteredCount > 0 && !allFilteredSelected
  }, [allFilteredSelected, selectedFilteredCount])
  const queueSummary = useMemo(() => {
    let importedPlays = 0
    let remainingPlays = 0
    let remaining = 0
    for (const item of queue) {
      importedPlays += item.importedPlayCount
      if (item.remaining) {
        remainingPlays += item.remainingPlayCount
        remaining += 1
      }
    }
    return { importedPlays, remainingPlays, remaining, reviewed: queue.length - remaining }
  }, [queue])
  const selectedPage = selected?.page
  const selectedPageRef = useRef<number | undefined>(undefined)
  selectedPageRef.current = selectedPage
  const sortRef = useRef<'plays' | 'artist' | 'batch' | 'lastPlayed'>('plays')
  sortRef.current = sort
  const queueFilterRef = useRef('')
  queueFilterRef.current = queueFilter
  const refresh = useCallback(async (strict = false): Promise<ImportQueueItem[]> => {
    const requestGeneration = beginImportRefresh(refreshGeneration)
    const startingPageGeneration = pageRequestGeneration.current
    const currentSelectedPage = selectedPageRef.current
    const currentSort = sortRef.current
    try {
      const { value: [nextState, nextQueue], applied } = await applyCurrentImportRefresh(requestGeneration, refreshGeneration, Promise.all([lastfmGateway.state(), loadImportQueue()]), ([currentState, currentQueue]) => {
        setState(currentState)
        setShowQueries(currentState.searchTerms)
        setQueue(currentQueue)
        setPendingDefaults(currentState.defaults)
      }, strict)
      if (!applied) return nextQueue
      const orderedNextQueue = sortImportQueue(nextQueue, currentSort)
      const navigableNextQueue = filterImportQueue(activeImportQueue(orderedNextQueue), queueFilterRef.current)
      const current = currentSelectedPage === undefined ? undefined : navigableNextQueue.find((item) => item.page === currentSelectedPage)
      const firstRemaining = navigableNextQueue[0]
      const target = current ?? ((nextState.phase === 'review' || nextState.phase === 'done') ? firstRemaining : undefined)
      if (startingPageGeneration !== pageRequestGeneration.current) return nextQueue
      if (target) {
        if (target.page !== currentSelectedPage) setSelected(target)
        const pageGeneration = ++pageRequestGeneration.current
        await applyCurrentImportPageResponse(pageGeneration, () => pageRequestGeneration.current, lastfmGateway.page({ batchId: target.page, artist: target.artist, album: target.album }), (nextPage) => setPage(pageWithQueuePosition(nextPage, navigableNextQueue)))
      } else if (isCurrentImportRefresh(requestGeneration, refreshGeneration) && nextState.phase !== 'review' && nextState.phase !== 'done') {
        setSelected(null)
        setPage(null)
      }
      return nextQueue
    } catch (reason) {
      if (isCurrentImportRefresh(requestGeneration, refreshGeneration)) reportError(reason)
      if (strict) throw reason
      return []
    }
  }, [reportError])
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
  const refreshQueueOnly = useCallback(async (strict = false): Promise<ImportQueueItem[]> => {
    const requestGeneration = beginImportRefresh(refreshGeneration)
    try {
      const { value: [, nextQueue] } = await applyCurrentImportRefresh(requestGeneration, refreshGeneration, Promise.all([lastfmGateway.state(), loadImportQueue()]), ([nextState, currentQueue]) => {
        setState(nextState)
        setShowQueries(nextState.searchTerms)
        setQueue(currentQueue)
        setPendingDefaults(nextState.defaults)
      }, strict)
      return nextQueue
    } catch (reason) {
      if (isCurrentImportRefresh(requestGeneration, refreshGeneration)) reportError(reason)
      if (strict) throw reason
      return []
    }
  }, [reportError])
  const skipQueueItem = async (item: ImportQueueItem) => {
    if (pageMutationRunning) return
    if (!item.remaining) {
      setShortcutStatus(item.status === 'done' ? 'That batch is already done.' : 'That batch is already ignored or excluded.')
      return
    }
    const action: ReviewAction = item.status === 'skipped' ? 'restore' : 'skip-album'
    queueMutationRunning.current = true
    setBusy(true)
    try {
      await lastfmGateway.review({ batchId: item.page, action, artist: item.artist, album: item.album })
      await refreshQueueOnly()
      setShortcutStatus(action === 'restore' ? 'Album resumed.' : 'Album skipped. Press S again to resume it.')
    } catch (reason) {
      reportError(reason)
    } finally {
      queueMutationRunning.current = false
      setBusy(false)
    }
  }
  useEffect(() => {
    let active = true
    const subscription = listen<ImportStateView>(lastfmEvents.changed, () => { if (shouldRefreshImportEvent(acceptAllRunning.current, queueMutationRunning.current)) void refresh() })
    const completions = listen<unknown>(lastfmEvents.applyFinished, (event) => {
      const result = parseImportApplyResult(event.payload)
      if (!result) {
        setError({ message: invalidApplyResultMessage, code: 'apply-failed', retryAt: null })
        void refreshQueueOnly()
      } else if (result.status === 'failed') {
        setError(result)
        void refreshQueueOnly()
      } else if (!advancingApply.current && result.batchId === selectedPageRef.current) {
        setError(null)
        void refresh()
      }
    })
    const installed = subscriptionsThenSnapshot([subscription, completions], refresh, () => active)
    return () => {
      active = false
      void installed.then((stop) => stop())
    }
  }, [refresh, refreshQueueOnly])
  useEffect(() => { setPage((current) => pageWithQueuePosition(current, filteredQueue)) }, [filteredQueue])
  useEffect(() => {
    if (!page || pageLoading || selected?.page !== page.batchId) return
    const next = nextRemainingImportQueue(filteredQueue, selected, sort)
    if (!next) return
    const transition = `${page.batchId}:${next.page}`
    if (prefetchedTransition.current === transition) return
    prefetchedTransition.current = transition
    // ponytail: one-batch lookahead; widen only if measured navigation still stalls.
    void lastfmGateway.page({ batchId: next.page, artist: next.artist, album: next.album }).catch(() => {
      if (prefetchedTransition.current === transition) prefetchedTransition.current = ''
    })
  }, [filteredQueue, page, pageLoading, selected, sort])
  useEffect(() => {
    let active = true
    const media = window.matchMedia('(prefers-color-scheme: dark)')
    let theme: Appearance['theme'] = 'system'
    const apply = (next: Appearance['theme']) => { theme = next; document.documentElement.dataset.theme = next === 'system' ? media.matches ? 'dark' : 'light' : next }
    const onMediaChange = () => apply(theme)
    apply('system')
    media.addEventListener('change', onMediaChange)
    const subscription = subscribeThenSnapshot(
      (install) => listen<Appearance>('appearance-changed', ({ payload }) => install(payload)),
      appGateway.appearance,
      (appearance) => apply(appearance.theme),
      () => active,
    )
    return () => {
      active = false
      media.removeEventListener('change', onMediaChange)
      void subscription.then((stop) => stop())
    }
  }, [])
  const start = async () => {
    if (!validImportIntent(pendingDefaults.importContent, pendingDefaults.includeHistoricalPlayCounts)) return
    setBusy(true); setError(null)
    try { await lastfmGateway.start(pendingDefaults); await refresh() } catch (reason) { reportError(reason) } finally { setBusy(false) }
  }
  const openQueueItem = async (item: ImportQueueItem, queueSnapshot = filteredQueue, focusQueue = false) => {
    setError(null)
    focusQueueAfterOpen.current = focusQueue
    const requestGeneration = pageRequestGeneration.current + 1
    setPageLoading(true)
    try {
      await loadSelectedImportPage(pageRequestGeneration, item, (target) => lastfmGateway.page({ batchId: target.page, artist: target.artist, album: target.album }), setSelected, (nextPage) => setPage(pageWithQueuePosition(nextPage, queueSnapshot)), () => setPage(null), () => setPageLoading(false))
    } catch (reason) {
      focusQueueAfterOpen.current = false
      if (isCurrentImportPageResponse(requestGeneration, pageRequestGeneration.current)) reportError(reason)
    }
  }
  const combineSelectedBatches = async () => {
    if (selectedBatchIds.length < 2 || pageMutationRunning) return
    queueMutationRunning.current = true
    setBusy(true)
    setError(null)
    try {
      const combined = await lastfmGateway.combineBatches(selectedBatchIds)
      const nextQueue = await refreshQueueOnly(true)
      setSelectedBatchPages(new Set())
      setQueueFilter('')
      if (!combined) throw new Error('The combined Last.fm batch is no longer available.')
      const navigable = activeImportQueue(sortImportQueue(nextQueue, sort))
      setSelected(navigable.find((item) => item.page === combined.batchId) ?? null)
      setPage(pageWithQueuePosition(combined, navigable))
      pageRequestGeneration.current += 1
    } catch (reason) {
      reportError(reason)
    } finally {
      queueMutationRunning.current = false
      setBusy(false)
    }
  }
  useLayoutEffect(() => {
    if (!focusQueueAfterOpen.current || pageLoading || !selected) return
    const target = document.querySelector<HTMLElement>(`[data-import-nav="queue"][aria-current="true"]`)
    if (!target) return
    focusQueueAfterOpen.current = false
    target.focus()
    target.scrollIntoView({ block: 'nearest' })
  }, [filteredQueue, pageLoading, selected])
  const nextQueueItem = (queueSnapshot = queue, focusQueue = false) => {
    const navigableSnapshot = filterImportQueue(activeImportQueue(sortImportQueue(queueSnapshot, sort)), queueFilter)
    const next = nextRemainingImportQueue(navigableSnapshot, selected, sort)
    if (next) {
      void openQueueItem(next, navigableSnapshot, focusQueue)
    }
    else { setSelected(null); setPage(null); void refresh() }
  }
  const acknowledgeReviewExclusion = (nextPage: PageView, remainingPlayCount: number, allExcluded: boolean) => {
    const nextQueue = projectImportQueueExclusion(queue, nextPage.batchId, remainingPlayCount, allExcluded)
    setState(nextPage.state)
    setQueue(nextQueue)
    setPage(pageWithQueuePosition(nextPage, filterImportQueue(activeImportQueue(sortImportQueue(nextQueue, sort)), queueFilter)))
    return nextQueue
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
    const navigableProjection = filterImportQueue(activeImportQueue(sortImportQueue(projection.queue, sort)), queueFilter)
    const next = nextRemainingImportQueue(navigableProjection, selected, sort)
    setQueue(projection.queue)
    setSelectedBatchPages((current) => { const next = new Set(current); next.delete(appliedPage); return next })
    setSelected(null)
    setPage(null)
    try {
      if (next) {
        await openQueueItem(next, navigableProjection, true)
      } else {
        focusQueueAfterOpen.current = false
      }
    } finally {
      advancingApply.current = false
    }
    void refreshQueueOnly(true).catch(() => {})
  }
  const previousQueueItem = () => {
    const index = selected ? filteredQueue.findIndex((item) => item.page === selected.page) : filteredQueue.length
    const previous = filteredQueue.slice(0, index).reverse().find((item) => item.remaining) ?? filteredQueue.slice(index + 1).reverse().find((item) => item.remaining)
    if (previous) void openQueueItem(previous, filteredQueue)
  }
  const acceptAll = async () => {
    if (pageMutationRunning) return
    setBusy(true); setError(null); acceptAllRunning.current = true
    try {
      const nextState = await lastfmGateway.acceptAll()
      setState(nextState)
      setAcceptAllOpen(false)
      setAcceptAllSummary(null)
      void refreshQueueOnly()
    } catch (reason) { reportError(reason) } finally { acceptAllRunning.current = false; setBusy(false) }
  }
  const prepareAcceptAll = async () => {
    if (pageMutationRunning) return
    setBusy(true); setError(null); acceptAllRunning.current = true
    try {
      const summary = await lastfmGateway.prepareAcceptAll()
      await refresh()
      setAcceptAllSummary(summary)
      setAcceptAllOpen(true)
    } catch (reason) { reportError(reason) } finally { acceptAllRunning.current = false; setBusy(false) }
  }
  const setSearchTerms = async (show: boolean) => {
    if (pageMutationRunning) return
    setShowQueries(show)
    setBusy(true)
    try { await lastfmGateway.setSearchTerms(show) } catch (reason) { reportError(reason) } finally { setBusy(false) }
  }
  const reviewReady = state.phase === 'review' || state.phase === 'done'
  const interactionBusy = busy || pageMutationRunning
  const emptyPage = importEmptyPageMessage(state.phase, pageLoading)
  const selectedError: DisplayError | null = selected?.error ? (() => {
    const code = importApplyErrorCode(selected.errorCode)
    const spotifyFailure = code === 'spotify-rate-limited' || code === 'spotify-quota-exhausted'
    return { message: selected.error, code, retryAt: spotifyFailure ? state.spotifyLimit?.deadline ?? null : selected.retryAt ?? null }
  })() : null
  const displayedError = error ?? selectedError
  const setBatchSelection = (pages: number[], checked: boolean) => setSelectedBatchPages((current) => {
    const next = new Set(current)
    for (const batchPage of pages) {
      if (checked) next.add(batchPage)
      else next.delete(batchPage)
    }
    return next
  })
  return <main className="lastfm-importer" aria-label="Last.fm importer">
    <header className="import-toolbar"><div><p className="eyebrow">LAST.FM HISTORY</p><h1>Last.fm importer</h1><p className="import-status" aria-live="polite">{state.applyingAll ? 'Applying confirmed Last.fm imports' : state.syncing ? 'Syncing new Last.fm plays' : importStatusText(state.phase, state.username, state.syncProblem)}{showsImportRemaining(state.phase) && state.remaining ? ` · ${state.remaining.toLocaleString()} left` : ''}{state.pendingReview && !state.remaining ? ` · ${state.pendingReview.toLocaleString()} pending review` : ''}</p></div><div className="import-toolbar-actions"><a href="https://www.last.fm/" onClick={(event) => { event.preventDefault(); void openExternalDestination({ kind: 'lastFm' }).catch(reportError) }}>Powered by Last.fm</a><button type="button" aria-keyshortcuts="?" disabled={acceptAllOpen} onClick={() => setShortcutsOpen(true)}>Keyboard shortcuts (?)</button>{reviewReady && <><span className="import-sort-label">Sort</span><div className="import-sort-control" role="group" aria-label="Queue sort">{([['plays', 'Most to import'], ['artist', 'Artist A–Z'], ['batch', 'Batch size'], ['lastPlayed', 'Last played']] as const).map(([value, label]) => <button type="button" key={value} disabled={interactionBusy} aria-pressed={sort === value} className={sort === value ? 'active' : ''} onClick={() => setSort(value)}>{label}</button>)}</div><label className="import-query-toggle"><input type="checkbox" aria-label="Show Spotify search terms" checked={showQueries} disabled={interactionBusy} onChange={(event) => void setSearchTerms(event.target.checked)} /> Show Spotify search terms</label><button type="button" disabled={interactionBusy || state.applyingAll || !state.remaining} onClick={() => void prepareAcceptAll()}>Accept All Imports…</button></>}</div></header>
    {displayedError ? <div className="import-error" role="alert"><span>{displayedError.message}</span><SpotifyLimitNotice code={displayedError.code} retryAt={displayedError.retryAt} /></div> : state.spotifyLimit && <div className="import-limit" role="status"><span>{state.spotifyLimit.kind === 'quota' ? 'Spotify Development Mode quota is cooling down.' : 'Spotify is rate limited.'}</span><SpotifyLimitNotice code={state.spotifyLimit.kind === 'quota' ? 'spotify-quota-exhausted' : 'spotify-rate-limited'} retryAt={state.spotifyLimit.deadline} /></div>}
    {state.phase === 'downloading' || state.phase === 'aggregating' || state.phase === null || state.phase === 'suspended' ? <DownloadPane state={state} defaults={pendingDefaults} busy={busy} onDefaults={setPendingDefaults} onStart={() => void start()} /> : <div className="import-workspace" aria-busy={pageLoading || state.applyingAll || pageMutationRunning}><aside className="import-queue" aria-label="Import queue"><div className="import-queue-header"><div><h2>Import queue</h2><small>{queueSummary.importedPlays.toLocaleString()} plays imported · {queueSummary.remainingPlays.toLocaleString()} remaining</small></div><span>{queueSummary.remaining} batches left</span></div><div className="import-queue-filter"><ImportQueueFilter value={queueFilter} onValue={setQueueFilter} /><div className="import-queue-bulk-actions"><label><input ref={selectAllBatches} type="checkbox" aria-label="Select all filtered batches" checked={allFilteredSelected} disabled={interactionBusy || !selectableFilteredQueue.length} onChange={(event) => setBatchSelection(selectableFilteredQueue.map((item) => item.page), event.currentTarget.checked)} /> Select all {selectableFilteredQueue.length.toLocaleString()} results</label><button type="button" disabled={interactionBusy || selectedBatchIds.length < 2} onClick={() => void combineSelectedBatches()}>Combine selected ({selectedBatchIds.length})</button></div></div>{filteredQueue.length || !queueFilter.trim() ? <VirtualQueue items={filteredQueue} selectedPage={selected?.page ?? null} selectedBatchPages={selectedBatchPages} disabled={interactionBusy || state.applyingAll} onOpen={(item) => void openQueueItem(item, filteredQueue, true)} onSelect={(batchPage, checked) => setBatchSelection([batchPage], checked)} onSkip={(item) => void skipQueueItem(item)} onTab={focusMappingFromQueue} onShortcuts={() => setShortcutsOpen(true)} /> : <p className="import-queue-empty" role="status">No matching batches.</p>}<div className="import-queue-progress"><progress max={queue.length || 1} value={queueSummary.reviewed} aria-label="Reviewed queue progress" /><span>Reviewed {queueSummary.reviewed} of {queue.length} batches</span></div></aside>{state.applyingAll ? <section className="import-empty"><strong>Applying confirmed imports…</strong><span>You can close this window; Retune will resume the queue after a restart.</span></section> : page ? <ImportPage page={page} failed={selected?.status === 'failed'} showQueries={showQueries} onRefresh={refresh} onRejected={acknowledgeReviewExclusion} onNext={nextQueueItem} onApplied={appliedAndAdvance} onPrevious={previousQueueItem} onError={reportError} onCollectionPage={(nextPage) => setPage(pageWithQueuePosition(nextPage, filteredQueue))} onTabToQueue={focusQueueTarget} onShortcuts={() => setShortcutsOpen(true)} onStatus={setShortcutStatus} onMutation={(running) => { queueMutationRunning.current = running; setPageMutationRunning(running) }} /> : <section className="import-empty"><strong>{emptyPage.title}</strong><span>{emptyPage.detail}</span></section>}</div>}
    <footer className="import-footer"><span>Historical import is an absolute baseline; incremental sync adds new plays, deduplicates Retune-origin scrobbles locally, and never erases existing plays.</span><span className="import-footer-hints">↑↓ move · Tab columns · Enter controls · E edit · Space toggle · X exclude · S skip/resume · A apply · ? shortcuts</span><span role="status" aria-live="polite">{shortcutStatus || (state.username ? `Last.fm: ${state.username}` : 'Account not connected')}</span></footer>
    {acceptAllOpen && acceptAllSummary && <AcceptAllDialog albumEntities={acceptAllSummary.albumEntities} trackEntities={acceptAllSummary.trackEntities} busy={interactionBusy} onCancel={() => { setAcceptAllOpen(false); setAcceptAllSummary(null) }} onConfirm={() => void acceptAll()} />}
    {shortcutsOpen && <KeyboardShortcutsDialog onCancel={() => setShortcutsOpen(false)} />}
  </main>
}
