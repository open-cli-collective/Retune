import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from 'react'
import { ModalDialog } from './viewShared.tsx'
import type { LastFmImportDefaults, LastFmImportState, Settings } from './types.ts'
import { applyCurrentImportPageResponse, canHandleImportShortcut, downloadAction, excludedImportCount, importAlbumActionAdvances, importDownloadCopy, importDownloadPercent, importEmptyPageMessage, importQueueHighlightIndex, importQueueVisibleRange, importStatusText, isCurrentImportPageResponse, loadSelectedImportPage, moveImportNavigationRow, moveImportQueueIndex, nextRemainingImportQueue, pickerCandidates, pickerSelectedUri, resolveImportCount, restPendingImportCount, selectedImportCount, selectedImportTrackConfidence, shouldRefreshImportEvent, showsImportRemaining, sortImportQueue, strongImportAlbumMatch, toggleImportRow, trackPickerQuery, validImportIntent, type CountMode, type ImportConfidence, type ImportNavigationTarget, type ImportPickerKind, type ImportQueueItem, type ImportQueuePage, type ImportSourceRow, type ReviewState } from './lastfmImportState.ts'
import './lastfmImporter.css'

type ImportStateView = LastFmImportState
type AlbumCandidate = { uri: string; name: string; artist: string; relation: 'best-match' | 'same-songs' | 'superset' | null; trackUris: string[]; trackNames: string[]; trackArtists: string[]; trackAlbums: string[] }
type MatchResult = { sourceId: string; searchTerm: string; confidence: 'exact' | 'likely' | 'low' | null; selectedUri: string | null; candidates: AlbumCandidate[]; trackMatches: Record<string, string> }
type PageItem = { source: ImportSourceRow; decision: { status: 'pending' | 'done' | 'skipped' | 'ignored-album' | 'ignored-artist'; excluded: boolean }; matchResult: MatchResult | null }
type PageView = { state: ImportStateView; batchId: number; artist: string; album: string; pageNumber: number; pageCount: number; rows: PageItem[]; options: { importContent: boolean; includeHistoricalPlayCounts: boolean; wholeAlbum: boolean; genre: string | null; rating: number | null; selectedTrackIds: string[] }; fuzzyGroups: Record<string, ImportSourceRow[]>; countModes: Record<string, CountMode>; lockedCountModes: string[] }
type PickerKind = ImportPickerKind
type PickerState = { kind: PickerKind; sourceId: string; query: string }
type FuzzyProps = { fuzzy?: ImportSourceRow[]; fuzzyTarget?: string; fuzzyExpanded: boolean; fuzzyMode: CountMode; fuzzyLocked: boolean; onFuzzyMode: (mode: CountMode) => void; onFuzzyToggle: () => void }
type ShortcutStatus = (message: string) => void

const IMPORT_NAV_KEYS = 'ArrowUp ArrowDown Tab Shift+Tab Enter E Space X S A Escape ?'

const emptyDefaults: LastFmImportDefaults = { importContent: true, includeHistoricalPlayCounts: true, wholeAlbum: false }
const emptyState: ImportStateView = { phase: null, username: null, spotifyAccountId: null, historyTo: null, downloadedThrough: null, nextPage: 1, totalPages: null, downloadedPages: 0, totalScrobbles: 0, includedScrobbles: 0, processedScrobbles: 0, defaults: emptyDefaults, remaining: 0, retryableError: null, searchTerms: true, syncing: false, lastSyncedAt: null, pendingReview: 0, syncProblem: null }
const importQueuePageLimit = 1000

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
  }
}

function confidenceLabel(confidence: MatchResult['confidence']) {
  return confidence === 'exact' ? 'EXACT TRACK MATCH' : confidence === 'likely' ? 'Likely' : confidence === 'low' ? 'Low' : 'Unmatched'
}

function relationLabel(relation: AlbumCandidate['relation']) {
  return relation === 'best-match' ? 'Best match' : relation === 'same-songs' ? 'Same songs' : relation === 'superset' ? 'Superset' : 'Unclassified'
}

function importNavigationTarget(element: EventTarget | null): HTMLElement | null {
  return element instanceof Element ? element.closest<HTMLElement>('[data-import-nav]') : null
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
  const target = document.querySelector<HTMLElement>(`[data-import-nav="${kind}"][data-import-row="${row}"]`)
  if (!target) return false
  target.focus()
  target.scrollIntoView({ block: 'nearest' })
  return true
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

function MatchPickerDialog({ kind, query: initialQuery, candidates, selectedUri, busy, onCancel, onSearch, onChoose }: { kind: PickerKind; query: string; candidates: AlbumCandidate[]; selectedUri: string | null; busy: boolean; onCancel: () => void; onSearch: (query: string) => void; onChoose: (uri: string) => void }) {
  const [query, setQuery] = useState(initialQuery)
  const [choice, setChoice] = useState(selectedUri ?? '')
  useEffect(() => { setQuery(initialQuery) }, [initialQuery])
  useEffect(() => { setChoice(selectedUri ?? '') }, [selectedUri])
  return <ModalDialog className="import-picker-dialog" labelledBy="import-picker-title" onCancel={onCancel} onSubmit={() => { if (choice) void onChoose(choice) }}>
    <header><p className="eyebrow">{kind === 'album' ? 'CHANGE ALBUM' : 'CHANGE TRACK'}</p><h2 id="import-picker-title">{kind === 'album' ? 'Choose a Spotify release' : 'Choose a Spotify track'}</h2></header>
    <div className="import-picker-search"><label htmlFor="import-picker-query">Search Spotify</label><div><input id="import-picker-query" autoFocus value={query} onChange={(event) => setQuery(event.target.value)} /><button type="button" disabled={busy || !query.trim()} onClick={() => onSearch(query)}>Search</button></div></div>
    <div className="import-picker-results" aria-live="polite">{candidates.length ? candidates.slice(0, 10).map((candidate) => <label className="import-picker-option" key={candidate.uri}><input type="radio" name="import-picker-choice" checked={choice === candidate.uri} onChange={() => setChoice(candidate.uri)} /><span><strong>{candidate.name}</strong><small>{candidate.artist}{kind === 'album' ? ` · ${candidate.trackUris.length} tracks` : candidate.trackAlbums[0] ? ` · ${candidate.trackAlbums[0]}` : ''}</small></span><em>{kind === 'album' ? relationLabel(candidate.relation) : confidenceLabel(candidate.relation === 'best-match' ? 'exact' : candidate.relation ? 'likely' : 'low')}</em></label>) : <p className="muted">Search to load up to 10 real Spotify candidates.</p>}</div>
    {kind === 'album' && <p className="import-picker-note">Counts follow the tracks you keep. Choosing a release remaps this page together.</p>}
    <footer><button type="button" onClick={onCancel}>Cancel</button><button type="submit" className="primary" disabled={busy || !choice}>Use This {kind === 'album' ? 'Album' : 'Track'}</button></footer>
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

function FuzzyPanel({ rows, targetUri, mode, expanded, locked, onMode, onToggle }: { rows: ImportSourceRow[]; targetUri: string; mode: CountMode; expanded: boolean; locked: boolean; onMode: (mode: CountMode) => void; onToggle: () => void }) {
  const variants = rows.flatMap((row) => row.variants)
  return <div className="import-fuzzy-panel"><div className="import-fuzzy-heading"><strong>COUNT MERGE</strong><span>{variants.length} Last.fm spellings/entries map to one Spotify track · {resolveImportCount(rows, mode).toLocaleString()} resulting plays</span>{locked && <small className="muted">Locked after import</small>}<button type="button" className="text-button" aria-expanded={expanded} onClick={onToggle}>{expanded ? 'Hide names' : 'Show names'}</button></div>{expanded && <div className="import-fuzzy-variants">{variants.map((variant, index) => <span key={`${variant.artist}${variant.album}${variant.track}${index}`}><span>{variant.artist} · {variant.album} · {variant.track}</span><strong>{variant.playCount.toLocaleString()}</strong></span>)}</div>}<fieldset disabled={locked} className="import-fuzzy-strategies" aria-label={`Play count strategy for ${targetUri}`}><legend>Play counts</legend>{(['sum', 'overwrite', 'zero'] as CountMode[]).map((value) => <label key={value}><input type="radio" name={`fuzzy-${targetUri}`} checked={mode === value} onChange={() => onMode(value)} />{value === 'sum' ? 'Sum' : value === 'overwrite' ? 'Use highest' : 'Zero'}</label>)}</fieldset></div>
}

function ImporterRow({ item, rowNumber, checked, fuzzy, fuzzyTarget, onToggle, onExclude, onChangeTrack, onFuzzyMode, onFuzzyToggle, fuzzyExpanded, fuzzyMode, fuzzyLocked, showQuery, onFocusRow }: { item: PageItem; rowNumber: number; checked: boolean; fuzzy?: ImportSourceRow[]; fuzzyTarget?: string; onToggle: () => void; onExclude: () => void; onChangeTrack: () => void; onFuzzyMode: (mode: CountMode) => void; onFuzzyToggle: () => void; fuzzyExpanded: boolean; fuzzyMode: CountMode; fuzzyLocked: boolean; showQuery: boolean; onFocusRow: (row: number) => void }) {
  const match = item.matchResult
  const track = matchedTrack(item)
  const trackConfidence: ImportConfidence = selectedImportTrackConfidence(item.source.stableId, match?.selectedUri ?? null, match?.trackMatches ?? {}, match?.confidence ?? null, match?.candidates ?? [])
  const excluded = item.decision.excluded
  const disabled = excluded || !['pending', 'skipped'].includes(item.decision.status)
  const excludeDisabled = !excluded && disabled
  return <article className={`import-track-row${excluded ? ' excluded' : ''}`}>
    <div className="import-source-cell import-nav-target" data-import-nav="source" data-import-row={rowNumber} tabIndex={0} aria-label={`Last.fm source ${item.source.track}`} aria-keyshortcuts={IMPORT_NAV_KEYS} onFocus={() => onFocusRow(rowNumber)}><button type="button" className="import-exclude-glyph" disabled={excludeDisabled} aria-label={excluded ? 'Undo exclusion' : `Exclude ${item.source.track}`} title={excluded ? 'Put this source row back in the queue' : 'Exclude this Last.fm source row'} onClick={onExclude}>{excluded ? '↺' : '⊘'}</button><label className="import-track-check"><input type="checkbox" aria-label={`Include ${item.source.track}`} checked={checked} disabled={disabled} onChange={onToggle} /><span /></label><div className="import-track-copy"><strong>{item.source.track}</strong><small>{item.source.playCount.toLocaleString()} plays · last {new Date(item.source.latest * 1000).toLocaleDateString()}</small>{excluded && <small className="import-excluded-copy">Excluded — won’t be imported or asked about again</small>}{fuzzy && fuzzyTarget && <FuzzyPanel rows={fuzzy} targetUri={fuzzyTarget} mode={fuzzyMode} locked={fuzzyLocked} expanded={fuzzyExpanded} onMode={onFuzzyMode} onToggle={onFuzzyToggle} />}</div></div>
    <div className="import-match-cell import-nav-target" data-import-nav="match" data-import-row={rowNumber} tabIndex={0} aria-label={`Spotify match for ${item.source.track}`} aria-keyshortcuts={IMPORT_NAV_KEYS} onFocus={() => onFocusRow(rowNumber)}>{track ? <><strong>{track.name}</strong><small>{track.artist} · {track.album}</small><span className={`confidence ${trackConfidence ?? 'low'}`}>{confidenceLabel(trackConfidence ?? 'low')}</span></> : <small className="muted">No supported match</small>}{showQuery && match?.searchTerm && <code>q={match.searchTerm}</code>}<button type="button" className="text-button" disabled={disabled} onClick={onChangeTrack}>Change Track…</button></div>
  </article>
}

function ImportPage({ page, showQueries, onRefresh, onNext, onPrevious, onError, onTabToQueue, onShortcuts, onStatus, onFocusRow }: { page: PageView; showQueries: boolean; onRefresh: () => Promise<ImportQueueItem[]>; onNext: (queue?: ImportQueueItem[]) => void; onPrevious: () => void; onError: (error: unknown) => void; onTabToQueue: () => boolean; onShortcuts: () => void; onStatus: ShortcutStatus; onFocusRow: (row: number) => void }) {
  const [review, setReview] = useState<ReviewState>(() => reviewForPage(page))
  const [busy, setBusy] = useState(false)
  const [picker, setPicker] = useState<PickerState | null>(null)
  const [expandedFuzzy, setExpandedFuzzy] = useState<Set<string>>(new Set())
  useEffect(() => { setReview(reviewForPage(page)); setExpandedFuzzy(new Set()) }, [page])
  const persist = async (next: ReviewState, refreshQueue = false) => {
    setReview(next)
    setBusy(true)
    try {
      await invoke('lastfm_import_options', { batchId: page.batchId, artist: page.artist, album: page.album, options: pageOptions(next) })
      if (refreshQueue) await onRefresh()
    } catch (error) { onError(error) } finally { setBusy(false) }
  }
  const run = async (command: string, args: Record<string, unknown>): Promise<ImportQueueItem[]> => {
    setBusy(true)
    try {
      await invoke(command, args)
      return await onRefresh()
    } catch (error) { onError(error); return [] } finally { setBusy(false) }
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
    setBusy(true)
    try {
      await invoke('lastfm_import_apply', { batchId: page.batchId, artist: page.artist, album: page.album, selectedIds: [...review.checked], options: pageOptions(review) })
      const nextQueue = await onRefresh()
      if (advance) onNext(nextQueue)
    } catch (error) { onError(error) } finally { setBusy(false) }
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
  const searchPicker = async (query: string) => {
    if (!picker) return
    const activePicker = picker
    await run(activePicker.kind === 'album' ? 'lastfm_import_change_album' : 'lastfm_import_change_track', { batchId: page.batchId, id: activePicker.sourceId, query })
    setPicker((current) => current && current.kind === activePicker.kind && current.sourceId === activePicker.sourceId ? { ...current, query } : current)
  }
  const choosePicker = async (uri: string) => {
    if (!picker) return
    const activePicker = picker
    await run('lastfm_import_select_match', { batchId: page.batchId, id: activePicker.sourceId, uri })
    setPicker((current) => current && current.kind === activePicker.kind && current.sourceId === activePicker.sourceId ? null : current)
  }
  const intentChange = (key: 'importContent' | 'includeHistoricalPlayCounts', checked: boolean) => {
    const next = { ...review, [key]: checked }
    if (!validImportIntent(next.importContent, next.includeHistoricalPlayCounts)) return
    if (!next.importContent) next.wholeAlbum = false
    void persist(next, true)
  }
  const fuzzy = (item: PageItem): FuzzyProps => {
    const group = fuzzyFor(item)
    if (!group) return { fuzzyExpanded: false, fuzzyMode: 'sum', fuzzyLocked: false, onFuzzyMode: () => {}, onFuzzyToggle: () => {} }
    const mode = page.countModes[group.target] ?? 'sum'
    return {
      fuzzy: group.group,
      fuzzyTarget: group.target,
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
      const next = moveImportNavigationRow(row, page.rows.length + 1, move)
      if (next !== row) {
        onFocusRow(next)
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
    if (event.key.toLowerCase() === 'e') {
      const item = row === 0 ? undefined : page.rows[row - 1]
      if (row === 0) openAlbumPicker()
      else if (item && !item.decision.excluded && ['pending', 'skipped'].includes(item.decision.status)) openTrackPicker(item.source.stableId)
      else onStatus('This source track is already done or excluded.')
      return
    }
    if (event.key === ' ') {
      if (row === 0) {
        if (!review.importContent) onStatus('Enable content import before selecting the whole album.')
        else void persist({ ...review, wholeAlbum: !review.wholeAlbum }, true)
        return
      }
      const item = page.rows[row - 1]
      if (item.decision.excluded || !['pending', 'skipped'].includes(item.decision.status)) onStatus('This source track cannot be selected.')
      else void persist(toggleImportRow(review, item.source.stableId), true)
      return
    }
    if (event.key.toLowerCase() === 'x') {
      const item = row === 0 ? undefined : page.rows[row - 1]
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
  return <section className="import-review" aria-labelledby="import-review-title" onKeyDown={handleNavigationKeyDown}>
    <header className="import-review-header"><div><p className="eyebrow">{page.artist}</p><h2 id="import-review-title">{page.album || 'Singles'}</h2><p className="import-page-meta">{page.rows.length} source tracks · {page.rows.reduce((total, item) => total + item.source.playCount, 0).toLocaleString()} plays</p></div><div className="import-page-actions"><button type="button" disabled={busy || page.pageNumber <= 1} aria-label="Previous batch" onClick={onPrevious}>‹</button><span>Batch {page.pageNumber} of {page.pageCount}</span><button type="button" disabled={busy || page.pageNumber >= page.pageCount} aria-label="Next batch" onClick={() => onNext()}>›</button><button type="button" disabled={busy} onClick={openAlbumPicker}>Change Album…</button><button type="button" disabled={busy} onClick={() => void toggleAlbumSkip()}>{canResumeAlbum ? 'Resume Album' : 'Skip Album'}</button><button type="button" disabled={busy} onClick={() => void run('lastfm_import_review', { batchId: page.batchId, action: 'ignore-album', artist: page.artist, album: page.album }).then(onNext)}>Ignore Album</button><button type="button" disabled={busy} onClick={() => void run('lastfm_import_review', { batchId: page.batchId, action: 'ignore-artist', artist: page.artist, album: page.album }).then(onNext)}>Ignore Artist</button></div></header>
    <div className="import-album-strip"><div className="import-nav-target" data-import-nav="source" data-import-row="0" tabIndex={0} aria-label="Last.fm album source" aria-keyshortcuts={IMPORT_NAV_KEYS} onFocus={() => onFocusRow(0)}><p>WHAT I’M IMPORTING</p><strong>{page.album || 'Singles'}</strong><small>{page.artist} · {page.rows.length} source tracks</small></div><div className="import-nav-target" data-import-nav="match" data-import-row="0" tabIndex={0} aria-label="Spotify album match" aria-keyshortcuts={IMPORT_NAV_KEYS} onFocus={() => onFocusRow(0)}><p>SPOTIFY MATCH</p><strong>{selectedAlbumName}</strong><small>{selectedAlbumCandidate ? relationLabel(selectedAlbumCandidate.relation) : 'No release selected'}</small>{albumAdvisory.strong && <span className="import-strong-match" role="status">STRONG MATCH{albumAdvisory.extraTrackCount ? ` · ${albumAdvisory.extraTrackCount} extra Spotify track${albumAdvisory.extraTrackCount === 1 ? '' : 's'}` : ''}</span>}<button type="button" disabled={busy} onClick={openAlbumPicker}>Change Album…</button></div></div>
    <div className="import-options" role="group" aria-label="Import options"><label><input type="checkbox" aria-label="Import tracks and albums found in history" checked={review.importContent} disabled={busy || (!review.includeHistoricalPlayCounts && review.importContent)} onChange={(event) => intentChange('importContent', event.target.checked)} /> Import tracks and albums found in history</label><label><input type="checkbox" aria-label="Include historical play counts" checked={review.includeHistoricalPlayCounts} disabled={busy || (!review.importContent && review.includeHistoricalPlayCounts)} onChange={(event) => intentChange('includeHistoricalPlayCounts', event.target.checked)} /> Include historical play counts</label><label><input type="checkbox" aria-label="Import whole album" checked={review.wholeAlbum} disabled={busy || !review.importContent} onChange={(event) => void persist({ ...review, wholeAlbum: event.target.checked }, true)} /> Import whole album</label><label>Genre <input aria-label="Import genre" value={review.genre} onChange={(event) => void persist({ ...review, genre: event.target.value })} placeholder="No change" /></label><label>Rating <select aria-label="Import rating" value={review.rating ?? ''} onChange={(event) => void persist({ ...review, rating: event.target.value ? Number(event.target.value) : null })}><option value="">No change</option>{[1, 2, 3, 4, 5].map((rating) => <option key={rating} value={rating}>{'★'.repeat(rating)}</option>)}</select></label></div>
    {review.wholeAlbum && <p className="import-exclusion-note">Exclude removes only this Last.fm source row. A track inherently included by the whole album cannot be removed from Spotify here.</p>}
    <div className="import-track-list">{page.rows.map((item, index) => { const itemFuzzy = fuzzy(item); return <ImporterRow key={item.source.stableId} item={item} rowNumber={index + 1} onFocusRow={onFocusRow} checked={review.checked.has(item.source.stableId)} showQuery={showQueries} onToggle={() => void persist(toggleImportRow(review, item.source.stableId), true)} onExclude={() => void run('lastfm_import_review', { batchId: page.batchId, id: item.source.stableId, action: item.decision.excluded ? 'undo-exclude' : 'exclude', artist: page.artist, album: page.album })} onChangeTrack={() => openTrackPicker(item.source.stableId)} {...itemFuzzy} fuzzyExpanded={itemFuzzy.fuzzyExpanded ?? false} fuzzyMode={itemFuzzy.fuzzyMode ?? 'sum'} fuzzyLocked={itemFuzzy.fuzzyLocked ?? false} onFuzzyMode={itemFuzzy.onFuzzyMode ?? (() => {})} onFuzzyToggle={itemFuzzy.onFuzzyToggle ?? (() => {})} /> })}</div>
    <footer className="import-review-footer"><span>{selectedImportCount(review)} selected · {excludedImportCount(review)} excluded · {restPendingImportCount(review)} rest pending</span><div><button type="button" disabled={busy || !selectedImportCount(review)} onClick={() => void apply(false)}>Accept Changes</button><button type="button" className="primary" disabled={busy || !selectedImportCount(review)} onClick={() => void apply(true)}>Accept &amp; Next Batch</button></div></footer>
    {picker && pickerItem && <MatchPickerDialog kind={picker.kind} query={picker.query} candidates={pickerCandidates(picker.kind, pickerMatch?.candidates ?? [])} selectedUri={pickerSelectedUri(picker.kind, picker.sourceId, pickerMatch?.selectedUri ?? null, pickerMatch?.trackMatches ?? {})} busy={busy} onCancel={() => setPicker(null)} onSearch={searchPicker} onChoose={choosePicker} />}
  </section>
}

const IMPORT_QUEUE_ROW_HEIGHT = 57
const IMPORT_QUEUE_OVERSCAN = 4

function VirtualQueue({ items, selectedPage, disabled, onOpen, onSkip, onTab, onShortcuts }: { items: ImportQueueItem[]; selectedPage: number | null; disabled: boolean; onOpen: (item: ImportQueueItem) => void; onSkip: (item: ImportQueueItem) => void; onTab: (shiftKey: boolean) => boolean; onShortcuts: () => void }) {
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
      const moved = onTab(event.shiftKey)
      if (moved) {
        event.preventDefault()
        event.stopPropagation()
      }
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
        {items.slice(range.start, range.end).map((item, index) => { const absoluteIndex = range.start + index; return <button type="button" data-import-nav="queue" data-queue-index={absoluteIndex} data-highlighted={highlightedIndex === absoluteIndex ? 'true' : undefined} aria-current={selectedPage === item.page ? 'true' : undefined} aria-keyshortcuts="ArrowUp ArrowDown Tab Shift+Tab Enter S ?" tabIndex={highlightedIndex === absoluteIndex ? 0 : -1} aria-label={`Batch ${absoluteIndex + 1} of ${items.length}: ${item.album || 'Singles'} by ${item.artist}, ${item.playCount.toLocaleString()} plays`} disabled={disabled} className={`import-queue-row${selectedPage === item.page ? ' selected' : ''}${highlightedIndex === absoluteIndex ? ' highlighted' : ''}`} key={item.page} onFocus={() => { highlightedPageRef.current = item.page; setHighlightedIndex((current) => current === absoluteIndex ? current : absoluteIndex) }} onKeyDown={(event) => handleKeyDown(event, item, absoluteIndex)} onClick={() => { highlightedPageRef.current = item.page; setHighlightedIndex(absoluteIndex); onOpen(item) }}><span className={`import-status-dot ${item.status ?? 'pending'}`} aria-label={item.status ?? 'pending'}>{item.status === 'done' ? '✓' : item.status === 'skipped' ? '–' : item.status === 'excluded' || item.status?.startsWith('ignored') ? '⊘' : '•'}</span><span className="import-queue-copy"><strong>{item.album || 'Singles'}</strong><small>{item.artist} · {item.sourceCount} tracks</small></span><span className="import-queue-count">{item.playCount.toLocaleString()}<small>plays</small></span></button> })}
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
  const [acceptAllOpen, setAcceptAllOpen] = useState(false)
  const [acceptAllSummary, setAcceptAllSummary] = useState<{ albumEntities: number; trackEntities: number } | null>(null)
  const [shortcutsOpen, setShortcutsOpen] = useState(false)
  const [shortcutStatus, setShortcutStatus] = useState('')
  const [pendingDefaults, setPendingDefaults] = useState<LastFmImportDefaults>(emptyDefaults)
  const pageRequestGeneration = useRef(0)
  const acceptAllRunning = useRef(false)
  const queueMutationRunning = useRef(false)
  const mappingRowRef = useRef(0)
  const orderedQueue = useMemo(() => sortImportQueue(queue, sort), [queue, sort])
  const queueSummary = useMemo(() => {
    let plays = 0
    let remaining = 0
    for (const item of queue) {
      plays += item.playCount
      if (item.remaining) remaining += 1
    }
    return { plays, remaining, reviewed: queue.length - remaining }
  }, [queue])
  const selectedPage = selected?.page
  const selectedPageRef = useRef<number | undefined>(undefined)
  selectedPageRef.current = selectedPage
  const sortRef = useRef<'plays' | 'artist' | 'batch' | 'lastPlayed'>('plays')
  sortRef.current = sort
  const refresh = useCallback(async (): Promise<ImportQueueItem[]> => {
    const requestGeneration = ++pageRequestGeneration.current
    const currentSelectedPage = selectedPageRef.current
    const currentSort = sortRef.current
    try {
      const [nextState, nextQueue] = await Promise.all([invoke<ImportStateView>('lastfm_import_state'), loadImportQueue()])
      setState(nextState)
      setShowQueries(nextState.searchTerms)
      setQueue(nextQueue)
      setPendingDefaults(nextState.defaults)
      const orderedNextQueue = sortImportQueue(nextQueue, currentSort)
      const current = currentSelectedPage === undefined ? undefined : nextQueue.find((item) => item.page === currentSelectedPage)
      const firstRemaining = orderedNextQueue.find((item) => item.remaining)
      const target = current ?? ((nextState.phase === 'review' || nextState.phase === 'done') ? firstRemaining : undefined)
      if (target) {
        if (target.page !== currentSelectedPage) setSelected(target)
        await applyCurrentImportPageResponse(requestGeneration, () => pageRequestGeneration.current, invoke<PageView | null>('lastfm_import_page', { batchId: target.page, artist: target.artist, album: target.album }), (nextPage) => setPage(pageWithQueuePosition(nextPage, orderedNextQueue)))
      } else if (isCurrentImportPageResponse(requestGeneration, pageRequestGeneration.current) && nextState.phase !== 'review' && nextState.phase !== 'done') {
        setSelected(null)
        setPage(null)
      }
      return nextQueue
    } catch (reason) {
      if (isCurrentImportPageResponse(requestGeneration, pageRequestGeneration.current)) setError(String(reason))
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
  const focusMappingFromQueue = (shiftKey: boolean) => focusImporterTarget(shiftKey ? 'match' : 'source', mappingRowRef.current)
  const refreshQueueOnly = async () => {
    const [nextState, nextQueue] = await Promise.all([invoke<ImportStateView>('lastfm_import_state'), loadImportQueue()])
    setState(nextState)
    setShowQueries(nextState.searchTerms)
    setQueue(nextQueue)
    setPendingDefaults(nextState.defaults)
    return nextQueue
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
    return () => { void subscription.then((stop) => stop()) }
  }, [refresh])
  useEffect(() => { setPage((current) => pageWithQueuePosition(current, orderedQueue)) }, [orderedQueue])
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
  const openQueueItem = async (item: ImportQueueItem, queueSnapshot = orderedQueue) => {
    const requestGeneration = pageRequestGeneration.current + 1
    setPageLoading(true)
    try {
      await loadSelectedImportPage(pageRequestGeneration, item, (target) => invoke<PageView | null>('lastfm_import_page', { batchId: target.page, artist: target.artist, album: target.album }), setSelected, (nextPage) => setPage(pageWithQueuePosition(nextPage, queueSnapshot)), () => setPage(null), () => setPageLoading(false))
    } catch (reason) {
      if (isCurrentImportPageResponse(requestGeneration, pageRequestGeneration.current)) setError(String(reason))
    }
  }
  const nextQueueItem = (queueSnapshot = queue) => {
    const orderedSnapshot = sortImportQueue(queueSnapshot, sort)
    const next = nextRemainingImportQueue(orderedSnapshot, selected, sort)
    if (next) void openQueueItem(next, orderedSnapshot)
    else { setSelected(null); setPage(null); void refresh() }
  }
  const previousQueueItem = () => {
    const index = selected ? orderedQueue.findIndex((item) => item.page === selected.page) : orderedQueue.length
    const previous = orderedQueue.slice(0, index).reverse().find((item) => item.remaining) ?? orderedQueue.slice(index + 1).reverse().find((item) => item.remaining)
    if (previous) void openQueueItem(previous)
  }
  const acceptAll = async () => {
    setBusy(true); setError(undefined); acceptAllRunning.current = true
    try {
      for (const item of orderedQueue.filter((entry) => entry.remaining)) await invoke('lastfm_import_accept_all_page', { batchId: item.page, artist: item.artist, album: item.album })
      await refresh()
      setAcceptAllOpen(false)
      setAcceptAllSummary(null)
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
    <header className="import-toolbar"><div><p className="eyebrow">LAST.FM HISTORY</p><h1>Last.fm importer</h1><p className="import-status" aria-live="polite">{state.syncing ? 'Syncing new Last.fm plays' : importStatusText(state.phase, state.username)}{showsImportRemaining(state.phase) && state.remaining ? ` · ${state.remaining.toLocaleString()} left` : ''}{state.pendingReview && !state.remaining ? ` · ${state.pendingReview.toLocaleString()} pending review` : ''}</p></div><div className="import-toolbar-actions"><a href="https://www.last.fm/" target="_blank" rel="noreferrer">Powered by Last.fm</a><button type="button" aria-keyshortcuts="?" disabled={acceptAllOpen} onClick={() => setShortcutsOpen(true)}>Keyboard shortcuts (?)</button>{reviewReady && <><span className="import-sort-label">Sort</span><div className="import-sort-control" role="group" aria-label="Queue sort">{([['plays', 'Most played'], ['artist', 'Artist A–Z'], ['batch', 'Batch size'], ['lastPlayed', 'Last played']] as const).map(([value, label]) => <button type="button" key={value} aria-pressed={sort === value} className={sort === value ? 'active' : ''} onClick={() => setSort(value)}>{label}</button>)}</div><label className="import-query-toggle"><input type="checkbox" aria-label="Show Spotify search terms" checked={showQueries} disabled={busy} onChange={(event) => void setSearchTerms(event.target.checked)} /> Show Spotify search terms</label><button type="button" disabled={busy || !state.remaining} onClick={() => void prepareAcceptAll()}>Accept All Imports…</button></>}</div></header>
    {error && <div className="import-error" role="alert">{error}</div>}
    {state.phase === 'downloading' || state.phase === 'aggregating' || state.phase === null || state.phase === 'suspended' ? <DownloadPane state={state} defaults={pendingDefaults} busy={busy} onDefaults={setPendingDefaults} onStart={() => void start()} /> : <div className="import-workspace" aria-busy={pageLoading}><aside className="import-queue" aria-label="Import queue"><div className="import-queue-header"><div><h2>Import queue</h2><small>{queue.length} batches · {queueSummary.plays.toLocaleString()} plays</small></div><span>{queueSummary.remaining} left</span></div><VirtualQueue items={orderedQueue} selectedPage={selected?.page ?? null} disabled={busy || pageLoading} onOpen={(item) => void openQueueItem(item)} onSkip={(item) => void skipQueueItem(item)} onTab={focusMappingFromQueue} onShortcuts={() => setShortcutsOpen(true)} /><div className="import-queue-progress"><progress max={queue.length || 1} value={queueSummary.reviewed} aria-label="Reviewed queue progress" /><span>Reviewed {queueSummary.reviewed} of {queue.length} batches</span></div></aside>{page ? <ImportPage page={page} showQueries={showQueries} onRefresh={refresh} onNext={nextQueueItem} onPrevious={previousQueueItem} onError={(reason) => setError(String(reason))} onTabToQueue={focusQueueTarget} onShortcuts={() => setShortcutsOpen(true)} onStatus={setShortcutStatus} onFocusRow={(row) => { mappingRowRef.current = row }} /> : <section className="import-empty"><strong>{emptyPage.title}</strong><span>{emptyPage.detail}</span></section>}</div>}
    <footer className="import-footer"><span>Historical import is an absolute baseline; incremental sync adds new plays, deduplicates Retune-origin scrobbles locally, and never erases existing plays.</span><span className="import-footer-hints">↑↓ move · Tab columns · Enter controls · E edit · Space toggle · X exclude · S skip/resume · A apply · ? shortcuts</span><span role="status" aria-live="polite">{shortcutStatus || (state.username ? `Last.fm: ${state.username}` : 'Account not connected')}</span></footer>
    {acceptAllOpen && acceptAllSummary && <AcceptAllDialog albumEntities={acceptAllSummary.albumEntities} trackEntities={acceptAllSummary.trackEntities} busy={busy} onCancel={() => { setAcceptAllOpen(false); setAcceptAllSummary(null) }} onConfirm={() => void acceptAll()} />}
    {shortcutsOpen && <KeyboardShortcutsDialog onCancel={() => setShortcutsOpen(false)} />}
  </main>
}
