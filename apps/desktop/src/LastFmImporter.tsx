import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { ModalDialog } from './viewShared.tsx'
import type { LastFmImportDefaults, Settings } from './types.ts'
import { excludedImportCount, isCurrentImportPageResponse, nextRemainingImportQueue, resolveImportCount, restPendingImportCount, selectedImportCount, sortImportQueue, toggleImportRow, validImportIntent, type CountMode, type ImportQueueItem, type ImportSourceRow, type ReviewState } from './lastfmImportState.ts'
import './lastfmImporter.css'

type ImportPhase = 'downloading' | 'matching' | 'review' | 'done' | 'suspended'
type ImportStateView = {
  phase: ImportPhase | null
  username: string | null
  spotifyAccountId: string | null
  nextPage: number
  totalPages: number | null
  totalScrobbles: number
  includedScrobbles: number
  matchedRows: number
  matchTotal: number
  defaults: LastFmImportDefaults
  remaining: number
  retryableError: { message: string; attempt: number; retryable: boolean } | null
  searchTerms: boolean
}
type AlbumCandidate = { uri: string; name: string; artist: string; relation: 'best-match' | 'same-songs' | 'superset' | null; trackUris: string[]; trackNames: string[]; trackArtists: string[]; trackAlbums: string[] }
type MatchResult = { sourceId: string; searchTerm: string; confidence: 'exact' | 'likely' | 'low' | null; selectedUri: string | null; candidates: AlbumCandidate[]; trackMatches: Record<string, string> }
type PageItem = { source: ImportSourceRow; decision: { status: 'pending' | 'done' | 'skipped' | 'ignored-album' | 'ignored-artist'; excluded: boolean }; matchResult: MatchResult | null }
type PageView = { state: ImportStateView; artist: string; album: string; pageNumber: number; pageCount: number; rows: PageItem[]; options: { importContent: boolean; includeHistoricalPlayCounts: boolean; wholeAlbum: boolean; genre: string | null; rating: number | null; selectedTrackIds: string[] }; fuzzyGroups: Record<string, ImportSourceRow[]>; countModes: Record<string, CountMode>; lockedCountModes: string[] }
type PickerKind = 'album' | 'track'
type PickerState = { kind: PickerKind; sourceId: string; query: string }
type FuzzyProps = { fuzzy?: ImportSourceRow[]; fuzzyTarget?: string; fuzzyExpanded: boolean; fuzzyMode: CountMode; fuzzyLocked: boolean; onFuzzyMode: (mode: CountMode) => void; onFuzzyToggle: () => void }

const emptyDefaults: LastFmImportDefaults = { importContent: true, includeHistoricalPlayCounts: true, wholeAlbum: false }
const emptyState: ImportStateView = { phase: null, username: null, spotifyAccountId: null, nextPage: 1, totalPages: null, totalScrobbles: 0, includedScrobbles: 0, matchedRows: 0, matchTotal: 0, defaults: emptyDefaults, remaining: 0, retryableError: null, searchTerms: true }

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

function pageWithQueuePosition(page: PageView | null, queue: ImportQueueItem[], sort: 'plays' | 'artist' | 'batch' | 'lastPlayed'): PageView | null {
  if (!page) return null
  const ordered = sortImportQueue(queue, sort)
  const index = ordered.findIndex((item) => item.artist === page.artist && item.album === page.album)
  return { ...page, pageNumber: index + 1, pageCount: ordered.length }
}

function statusText(state: ImportStateView) {
  if (state.phase === 'downloading') return `Downloading Last.fm history · page ${state.nextPage}${state.totalPages ? ` of ${state.totalPages}` : ''}`
  if (state.phase === 'matching') return `Matching Last.fm history · ${state.matchedRows.toLocaleString()} of ${state.matchTotal.toLocaleString()} tracks`
  if (state.phase === 'suspended') return 'Import suspended for account safety'
  if (state.phase === 'done') return 'Import complete'
  return state.username ? `Ready to review ${state.includedScrobbles.toLocaleString()} scrobbles` : 'Connect Last.fm and Spotify to begin'
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
  return confidence === 'exact' ? 'Exact' : confidence === 'likely' ? 'Likely' : confidence === 'low' ? 'Low' : 'Unmatched'
}

function relationLabel(relation: AlbumCandidate['relation']) {
  return relation === 'best-match' ? 'Best match' : relation === 'same-songs' ? 'Same songs' : relation === 'superset' ? 'Superset' : 'Unclassified'
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
  const total = state.totalPages ?? 0
  const downloaded = Math.max(0, state.nextPage - 1)
  const percent = total ? Math.min(100, Math.round((downloaded / total) * 100)) : 0
  const isSetup = state.phase === null
  const isSuspended = state.phase === 'suspended'
  return <section className="import-progress-pane" aria-labelledby="import-progress-title">
    <div className="import-progress-copy">
      <p className="eyebrow">LAST.FM HISTORY</p>
      <h2 id="import-progress-title">{isSetup ? 'Import your complete Last.fm history' : isSuspended ? 'Import suspended for account safety' : 'Downloading your Last.fm history'}</h2>
      <p>{isSetup ? 'Retune takes a fixed snapshot and saves it page by page. You can review every match before anything is applied.' : isSuspended ? 'Reconnect the saved Last.fm and Spotify accounts before resuming this session.' : `Page ${state.nextPage}${state.totalPages ? ` of ${state.totalPages}` : ''} · ${state.includedScrobbles.toLocaleString()} scrobbles saved so far`}</p>
      {!isSetup && !isSuspended && <><progress max={100} value={percent} aria-label="Last.fm download progress" /><span className="import-progress-label">{total ? `${downloaded} of ${total} pages downloaded` : 'Discovering the history size…'}</span></>}
      <ImportIntentChecks defaults={isSetup ? defaults : state.defaults} disabled={!isSetup || busy} onChange={onDefaults} />
      <p className="import-leave-running">You can leave this running — Retune keeps playing, and matching starts as soon as the history lands.</p>
      {state.retryableError && <p className="import-error" role="alert">{state.retryableError.message} {state.retryableError.retryable ? `Attempt ${state.retryableError.attempt}. Resume to retry.` : ''}</p>}
      <button type="button" className="primary" disabled={busy} onClick={onStart}>{isSetup ? 'Start import' : isSuspended ? 'Check accounts and resume' : 'Resume download'}</button>
    </div>
  </section>
}

function MatchingPane({ state }: { state: ImportStateView }) {
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string>()
  const percent = state.matchTotal ? Math.round((state.matchedRows / state.matchTotal) * 100) : 0
  const resume = async () => {
    setBusy(true)
    setError(undefined)
    try { await invoke('start_lastfm_import', { defaults: state.defaults }) } catch (reason) { setError(String(reason)) } finally { setBusy(false) }
  }
  return <section className="import-progress-pane" aria-labelledby="matching-progress-title">
    <div className="import-progress-copy">
      <p className="eyebrow">SPOTIFY MATCH</p>
      <h2 id="matching-progress-title">Matching your Last.fm history to Spotify</h2>
      <p>Retune searches sequentially and saves each result so you can leave and resume safely.</p>
      <progress max={100} value={percent} aria-label="Spotify matching progress" />
      <span className="import-progress-label">{state.matchedRows.toLocaleString()} of {state.matchTotal.toLocaleString()} source tracks matched</span>
      <p className="import-leave-running">You can leave this running — the review queue appears when matching is complete.</p>
      {state.retryableError && <p className="import-error" role="alert">{state.retryableError.message}</p>}
      {error && <p className="import-error" role="alert">{error}</p>}
      <button type="button" className="primary" disabled={busy} onClick={() => void resume()}>{busy ? 'Resuming…' : 'Resume matching'}</button>
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

function FuzzyPanel({ rows, targetUri, mode, expanded, locked, onMode, onToggle }: { rows: ImportSourceRow[]; targetUri: string; mode: CountMode; expanded: boolean; locked: boolean; onMode: (mode: CountMode) => void; onToggle: () => void }) {
  const variants = rows.flatMap((row) => row.variants)
  return <div className="import-fuzzy-panel"><div className="import-fuzzy-heading"><strong>FUZZY</strong><span>{variants.length} raw spellings · {resolveImportCount(rows, mode).toLocaleString()} resulting plays</span>{locked && <small className="muted">Locked after import</small>}<button type="button" className="text-button" aria-expanded={expanded} onClick={onToggle}>{expanded ? 'Hide names' : 'Show names'}</button></div>{expanded && <div className="import-fuzzy-variants">{variants.map((variant, index) => <span key={`${variant.artist}${variant.album}${variant.track}${index}`}><span>{variant.artist} · {variant.album} · {variant.track}</span><strong>{variant.playCount.toLocaleString()}</strong></span>)}</div>}<fieldset disabled={locked} className="import-fuzzy-strategies" aria-label={`Play count strategy for ${targetUri}`}><legend>Play counts</legend>{(['sum', 'overwrite', 'zero'] as CountMode[]).map((value) => <label key={value}><input type="radio" name={`fuzzy-${targetUri}`} checked={mode === value} onChange={() => onMode(value)} />{value === 'sum' ? 'Sum' : value === 'overwrite' ? 'Overwrite' : 'Zero'}</label>)}</fieldset></div>
}

function ImporterRow({ item, checked, fuzzy, fuzzyTarget, onToggle, onExclude, onChangeTrack, onFuzzyMode, onFuzzyToggle, fuzzyExpanded, fuzzyMode, fuzzyLocked, showQuery }: { item: PageItem; checked: boolean; fuzzy?: ImportSourceRow[]; fuzzyTarget?: string; onToggle: () => void; onExclude: () => void; onChangeTrack: () => void; onFuzzyMode: (mode: CountMode) => void; onFuzzyToggle: () => void; fuzzyExpanded: boolean; fuzzyMode: CountMode; fuzzyLocked: boolean; showQuery: boolean }) {
  const match = item.matchResult
  const track = matchedTrack(item)
  const excluded = item.decision.excluded
  const disabled = excluded || item.decision.status === 'done'
  return <article className={`import-track-row${excluded ? ' excluded' : ''}`}>
    <div className="import-source-cell"><button type="button" className="import-exclude-glyph" aria-label={excluded ? 'Undo exclusion' : `Exclude ${item.source.track}`} title={excluded ? 'Put this source row back in the queue' : 'Exclude this Last.fm source row'} onClick={onExclude}>{excluded ? '↺' : '⊘'}</button><label className="import-track-check"><input type="checkbox" aria-label={`Include ${item.source.track}`} checked={checked} disabled={disabled} onChange={onToggle} /><span /></label><div className="import-track-copy"><strong>{item.source.track}</strong><small>{item.source.playCount.toLocaleString()} plays · last {new Date(item.source.latest * 1000).toLocaleDateString()}</small>{excluded && <small className="import-excluded-copy">Excluded — won’t be imported or asked about again</small>}{fuzzy && fuzzyTarget && <FuzzyPanel rows={fuzzy} targetUri={fuzzyTarget} mode={fuzzyMode} locked={fuzzyLocked} expanded={fuzzyExpanded} onMode={onFuzzyMode} onToggle={onFuzzyToggle} />}</div></div>
    <div className="import-match-cell">{track ? <><strong>{track.name}</strong><small>{track.artist} · {track.album}</small><span className={`confidence ${match?.confidence ?? 'low'}`}>{confidenceLabel(match?.confidence ?? 'low')}</span></> : <small className="muted">No supported match</small>}{showQuery && match?.searchTerm && <code>q={match.searchTerm}</code>}<button type="button" className="text-button" disabled={disabled} onClick={onChangeTrack}>Change Track…</button></div>
  </article>
}

function ImportPage({ page, showQueries, onRefresh, onNext, onPrevious, onError }: { page: PageView; showQueries: boolean; onRefresh: () => Promise<ImportQueueItem[]>; onNext: (queue?: ImportQueueItem[]) => void; onPrevious: () => void; onError: (error: unknown) => void }) {
  const [review, setReview] = useState<ReviewState>(() => reviewForPage(page))
  const [busy, setBusy] = useState(false)
  const [picker, setPicker] = useState<PickerState | null>(null)
  const [expandedFuzzy, setExpandedFuzzy] = useState<Set<string>>(new Set())
  useEffect(() => { setReview(reviewForPage(page)); setExpandedFuzzy(new Set()) }, [page])
  const persist = async (next: ReviewState, refreshQueue = false) => {
    setReview(next)
    setBusy(true)
    try {
      await invoke('lastfm_import_options', { artist: page.artist, album: page.album, options: pageOptions(next) })
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
  const apply = async (advance: boolean) => {
    setBusy(true)
    try {
      await invoke('lastfm_import_apply', { artist: page.artist, album: page.album, selectedIds: [...review.checked], options: pageOptions(review) })
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
    setPicker({ kind: 'track', sourceId, query: item?.matchResult?.searchTerm ?? item?.source.track ?? '' })
  }
  const openAlbumPicker = () => setPicker({ kind: 'album', sourceId: page.rows[0]?.source.stableId ?? '', query: page.album })
  const searchPicker = async (query: string) => {
    if (!picker) return
    await run(picker.kind === 'album' ? 'lastfm_import_change_album' : 'lastfm_import_change_track', { id: picker.sourceId, query })
    setPicker({ ...picker, query })
  }
  const choosePicker = async (uri: string) => {
    if (!picker) return
    await run('lastfm_import_select_match', { id: picker.sourceId, uri })
    setPicker(null)
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
  return <section className="import-review" aria-labelledby="import-review-title">
    <header className="import-review-header"><div><p className="eyebrow">{page.artist}</p><h2 id="import-review-title">{page.album || 'Singles'}</h2><p className="import-page-meta">{page.rows.length} source tracks · {page.rows.reduce((total, item) => total + item.source.playCount, 0).toLocaleString()} plays</p></div><div className="import-page-actions"><button type="button" disabled={busy || page.pageNumber <= 1} aria-label="Previous album" onClick={onPrevious}>‹</button><span>Page {page.pageNumber} of {page.pageCount}</span><button type="button" disabled={busy || page.pageNumber >= page.pageCount} aria-label="Next album" onClick={() => onNext()}>›</button><button type="button" disabled={busy} onClick={openAlbumPicker}>Change Album…</button><button type="button" disabled={busy} onClick={() => void run('lastfm_import_review', { id: page.rows[0]?.source.stableId ?? '', action: 'skip-album', artist: page.artist, album: page.album }).then(onNext)}>Skip Album</button><button type="button" disabled={busy} onClick={() => void run('lastfm_import_review', { id: page.rows[0]?.source.stableId ?? '', action: 'ignore-album', artist: page.artist, album: page.album }).then(onNext)}>Ignore Album</button><button type="button" disabled={busy} onClick={() => void run('lastfm_import_review', { id: page.rows[0]?.source.stableId ?? '', action: 'ignore-artist', artist: page.artist, album: page.album }).then(onNext)}>Ignore Artist</button></div></header>
    <div className="import-album-strip"><div><p>WHAT I’M IMPORTING</p><strong>{page.album || 'Singles'}</strong><small>{page.artist} · {page.rows.length} source tracks</small></div><div><p>SPOTIFY MATCH</p><strong>{page.rows[0]?.matchResult?.candidates.find((candidate) => candidate.uri === page.rows[0]?.matchResult?.selectedUri)?.name ?? 'Choose a release'}</strong><small>{page.rows[0]?.matchResult?.confidence ? confidenceLabel(page.rows[0].matchResult.confidence) : 'No release selected'}</small><button type="button" disabled={busy} onClick={openAlbumPicker}>Change Album…</button></div></div>
    <div className="import-options" role="group" aria-label="Import options"><label><input type="checkbox" aria-label="Import tracks and albums found in history" checked={review.importContent} disabled={busy || (!review.includeHistoricalPlayCounts && review.importContent)} onChange={(event) => intentChange('importContent', event.target.checked)} /> Import tracks and albums found in history</label><label><input type="checkbox" aria-label="Include historical play counts" checked={review.includeHistoricalPlayCounts} disabled={busy || (!review.importContent && review.includeHistoricalPlayCounts)} onChange={(event) => intentChange('includeHistoricalPlayCounts', event.target.checked)} /> Include historical play counts</label><label><input type="checkbox" aria-label="Import whole album" checked={review.wholeAlbum} disabled={busy || !review.importContent} onChange={(event) => void persist({ ...review, wholeAlbum: event.target.checked }, true)} /> Import whole album</label><label>Genre <input aria-label="Import genre" value={review.genre} onChange={(event) => void persist({ ...review, genre: event.target.value })} placeholder="No change" /></label><label>Rating <select aria-label="Import rating" value={review.rating ?? ''} onChange={(event) => void persist({ ...review, rating: event.target.value ? Number(event.target.value) : null })}><option value="">No change</option>{[1, 2, 3, 4, 5].map((rating) => <option key={rating} value={rating}>{'★'.repeat(rating)}</option>)}</select></label></div>
    {review.wholeAlbum && <p className="import-exclusion-note">Exclude removes only this Last.fm source row. A track inherently included by the whole album cannot be removed from Spotify here.</p>}
    <div className="import-track-list">{page.rows.map((item) => <ImporterRow key={item.source.stableId} item={item} checked={review.checked.has(item.source.stableId)} showQuery={showQueries} onToggle={() => void persist(toggleImportRow(review, item.source.stableId), true)} onExclude={() => void run('lastfm_import_review', { id: item.source.stableId, action: item.decision.excluded ? 'undo-exclude' : 'exclude', artist: page.artist, album: page.album })} onChangeTrack={() => openTrackPicker(item.source.stableId)} {...fuzzy(item)} fuzzyExpanded={fuzzy(item).fuzzyExpanded ?? false} fuzzyMode={fuzzy(item).fuzzyMode ?? 'sum'} fuzzyLocked={fuzzy(item).fuzzyLocked ?? false} onFuzzyMode={fuzzy(item).onFuzzyMode ?? (() => {})} onFuzzyToggle={fuzzy(item).onFuzzyToggle ?? (() => {})} />)}</div>
    <footer className="import-review-footer"><span>{selectedImportCount(review)} selected · {excludedImportCount(review)} excluded · {restPendingImportCount(review)} rest pending</span><div><button type="button" disabled={busy || !selectedImportCount(review)} onClick={() => void apply(false)}>Accept Changes</button><button type="button" className="primary" disabled={busy || !selectedImportCount(review)} onClick={() => void apply(true)}>Accept &amp; Next Album</button></div></footer>
    {picker && pickerItem && <MatchPickerDialog kind={picker.kind} query={picker.query} candidates={pickerMatch?.candidates ?? []} selectedUri={pickerMatch?.selectedUri ?? null} busy={busy} onCancel={() => setPicker(null)} onSearch={searchPicker} onChoose={choosePicker} />}
  </section>
}

export default function LastFmImporter() {
  const [state, setState] = useState(emptyState)
  const [queue, setQueue] = useState<ImportQueueItem[]>([])
  const [sort, setSort] = useState<'plays' | 'artist' | 'batch' | 'lastPlayed'>('plays')
  const [showQueries, setShowQueries] = useState(true)
  const [selected, setSelected] = useState<ImportQueueItem | null>(null)
  const [page, setPage] = useState<PageView | null>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string>()
  const [acceptAllOpen, setAcceptAllOpen] = useState(false)
  const [pendingDefaults, setPendingDefaults] = useState<LastFmImportDefaults>(emptyDefaults)
  const pageRequestGeneration = useRef(0)
  const orderedQueue = useMemo(() => sortImportQueue(queue, sort), [queue, sort])
  const selectedArtist = selected?.artist
  const selectedAlbum = selected?.album
  const refresh = useCallback(async (): Promise<ImportQueueItem[]> => {
    const requestGeneration = ++pageRequestGeneration.current
    try {
      const [nextState, nextQueue] = await Promise.all([invoke<ImportStateView>('lastfm_import_state'), invoke<ImportQueueItem[]>('lastfm_import_queue')])
      setState(nextState)
      setShowQueries(nextState.searchTerms)
      setQueue(nextQueue)
      setPendingDefaults(nextState.defaults)
      const current = selectedArtist && selectedAlbum ? nextQueue.find((item) => item.artist === selectedArtist && item.album === selectedAlbum) : undefined
      const firstRemaining = sortImportQueue(nextQueue, sort).find((item) => item.remaining)
      const target = current ?? ((nextState.phase === 'review' || nextState.phase === 'done') ? firstRemaining : undefined)
      if (target) {
        if (target.artist !== selectedArtist || target.album !== selectedAlbum) setSelected(target)
        const nextPage = await invoke<PageView | null>('lastfm_import_page', { artist: target.artist, album: target.album })
        if (isCurrentImportPageResponse(requestGeneration, pageRequestGeneration.current)) setPage(pageWithQueuePosition(nextPage, nextQueue, sort))
      } else if (isCurrentImportPageResponse(requestGeneration, pageRequestGeneration.current) && nextState.phase !== 'review' && nextState.phase !== 'done') {
        setSelected(null)
        setPage(null)
      }
      return nextQueue
    } catch (reason) {
      if (isCurrentImportPageResponse(requestGeneration, pageRequestGeneration.current)) setError(String(reason))
      return []
    }
  }, [selectedArtist, selectedAlbum, sort])
  useEffect(() => {
    void refresh()
    const subscription = listen<ImportStateView>('lastfm-import-changed', () => { void refresh() })
    return () => { void subscription.then((stop) => stop()) }
  }, [refresh])
  useEffect(() => { setPage((current) => pageWithQueuePosition(current, queue, sort)) }, [queue, sort])
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
  const openQueueItem = async (item: ImportQueueItem, queueSnapshot = queue) => {
    const requestGeneration = ++pageRequestGeneration.current
    setSelected(item)
    try {
      const nextPage = await invoke<PageView | null>('lastfm_import_page', { artist: item.artist, album: item.album })
      if (isCurrentImportPageResponse(requestGeneration, pageRequestGeneration.current)) setPage(pageWithQueuePosition(nextPage, queueSnapshot, sort))
    } catch (reason) {
      if (isCurrentImportPageResponse(requestGeneration, pageRequestGeneration.current)) setError(String(reason))
    }
  }
  const nextQueueItem = (queueSnapshot = queue) => {
    const next = nextRemainingImportQueue(queueSnapshot, selected, sort)
    if (next) void openQueueItem(next, queueSnapshot)
    else { setSelected(null); setPage(null); void refresh() }
  }
  const previousQueueItem = () => {
    const index = selected ? orderedQueue.findIndex((item) => item.artist === selected.artist && item.album === selected.album) : orderedQueue.length
    const previous = orderedQueue.slice(0, index).reverse().find((item) => item.remaining) ?? orderedQueue.slice(index + 1).reverse().find((item) => item.remaining)
    if (previous) void openQueueItem(previous)
  }
  const acceptAll = async () => {
    setBusy(true); setError(undefined)
    try {
      for (const item of orderedQueue.filter((entry) => entry.remaining)) { await invoke('lastfm_import_accept_all_page', { artist: item.artist, album: item.album }); await refresh() }
      setAcceptAllOpen(false)
    } catch (reason) { setError(String(reason)) } finally { setBusy(false) }
  }
  const setSearchTerms = async (show: boolean) => {
    setShowQueries(show)
    setBusy(true)
    try { await invoke('lastfm_import_search_terms', { show }) } catch (reason) { setError(String(reason)) } finally { setBusy(false) }
  }
  const reviewReady = state.phase === 'review' || state.phase === 'done'
  const albumEntities = orderedQueue.filter((item) => item.remaining).reduce((total, item) => total + item.albumEntities, 0)
  const trackEntities = orderedQueue.filter((item) => item.remaining).reduce((total, item) => total + item.trackEntities, 0)
  return <main className="lastfm-importer" aria-label="Last.fm importer">
    <header className="import-toolbar"><div><p className="eyebrow">LAST.FM HISTORY</p><h1>Last.fm importer</h1><p className="import-status" aria-live="polite">{statusText(state)}{state.remaining ? ` · ${state.remaining.toLocaleString()} left` : ''}</p></div><div className="import-toolbar-actions"><a href="https://www.last.fm/" target="_blank" rel="noreferrer">Powered by Last.fm</a>{reviewReady && <><span className="import-sort-label">Sort</span><div className="import-sort-control" role="group" aria-label="Queue sort">{([['plays', 'Most played'], ['artist', 'Artist A–Z'], ['batch', 'Batch size'], ['lastPlayed', 'Last played']] as const).map(([value, label]) => <button type="button" key={value} aria-pressed={sort === value} className={sort === value ? 'active' : ''} onClick={() => setSort(value)}>{label}</button>)}</div><label className="import-query-toggle"><input type="checkbox" aria-label="Show Spotify search terms" checked={showQueries} disabled={busy} onChange={(event) => void setSearchTerms(event.target.checked)} /> Show Spotify search terms</label><button type="button" disabled={busy || !state.remaining} onClick={() => setAcceptAllOpen(true)}>Accept All Imports…</button></>}{!reviewReady && state.phase !== 'downloading' && state.phase !== 'matching' && <button type="button" className="primary" disabled={busy} onClick={() => void start()}>Resume import</button>}</div></header>
    {error && <div className="import-error" role="alert">{error}</div>}
    {state.phase === 'downloading' || state.phase === null || state.phase === 'suspended' ? <DownloadPane state={state} defaults={pendingDefaults} busy={busy} onDefaults={setPendingDefaults} onStart={() => void start()} /> : state.phase === 'matching' ? <MatchingPane state={state} /> : <div className="import-workspace"><aside className="import-queue" aria-label="Import queue"><div className="import-queue-header"><div><h2>Import queue</h2><small>{queue.length} albums · {queue.reduce((total, item) => total + item.playCount, 0).toLocaleString()} plays</small></div><span>{queue.filter((item) => item.remaining).length} left</span></div><div className="import-queue-list">{orderedQueue.map((item) => <button type="button" className={`import-queue-row${selected?.artist === item.artist && selected.album === item.album ? ' selected' : ''}`} key={`${item.artist}${item.album}`} onClick={() => void openQueueItem(item)}><span className={`import-status-dot ${item.status ?? 'pending'}`} aria-label={item.status ?? 'pending'}>{item.status === 'done' ? '✓' : item.status === 'skipped' ? '–' : item.status === 'excluded' || item.status?.startsWith('ignored') ? '⊘' : '•'}</span><span className="import-queue-copy"><strong>{item.album || 'Singles'}</strong><small>{item.artist} · {item.sourceIds.length} tracks</small></span><span className="import-queue-count">{item.playCount.toLocaleString()}<small>plays</small></span></button>)}</div><div className="import-queue-progress"><progress max={queue.length || 1} value={queue.filter((item) => !item.remaining).length} aria-label="Reviewed queue progress" /><span>Reviewed {queue.filter((item) => !item.remaining).length} of {queue.length} albums</span></div></aside>{page ? <ImportPage page={page} showQueries={showQueries} onRefresh={refresh} onNext={nextQueueItem} onPrevious={previousQueueItem} onError={(reason) => setError(String(reason))} /> : <section className="import-empty"><strong>No review page selected</strong><span>Select an album from the queue.</span></section>}</div>}
    <footer className="import-footer"><span>Last.fm is an absolute historical baseline. Existing Retune plays are never erased.</span><span>{state.username ? `Last.fm: ${state.username}` : 'Account not connected'}</span></footer>
    {acceptAllOpen && <AcceptAllDialog albumEntities={albumEntities} trackEntities={trackEntities} busy={busy} onCancel={() => setAcceptAllOpen(false)} onConfirm={() => void acceptAll()} />}
  </main>
}
