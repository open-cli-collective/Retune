import { useEffect, useRef, useState } from 'react'
import { libraryGateway } from './libraryGateway.ts'
import { spotifyGateway } from './spotifyGateway.ts'
import type { DecisionTrack, LibrarySources, MergePlayCount, Track, TrackInfo, TrackMergeEdit, TrackMergePreview, SearchTrack } from './types.ts'
import { ModalDialog } from './viewShared.tsx'
import { beginRequestGeneration, formatTime } from './ui.ts'
import './trackDecisions.css'

const date = (value: number | null) => value === null ? 'Unknown' : new Date(value * 1000).toLocaleDateString()

export function SourcesList({ sources }: { sources: LibrarySources }) {
  return <ul className="library-sources">
    {sources.localFile && <li>Local audio file</li>}
    {sources.savedTrack && <li>Individually saved in Spotify</li>}
    {sources.savedAlbums.map((album, index) => <li key={`${album}:${index}`}>Saved Spotify album · {album}</li>)}
    {(sources.retained || (sources.membershipKnown && !sources.savedTrack && !sources.savedAlbums.length && !sources.localFile)) && <li>Kept in Retune</li>}
    {!sources.membershipKnown && !sources.localFile && <li>Spotify membership incomplete · sync to check all sources</li>}
  </ul>
}

function TrackSummary({ track }: { track: Pick<DecisionTrack, 'name' | 'art' | 'alb' | 'durationSecs' | 'playCount'> }) {
  return <><strong>{track.name}</strong><span>{track.art} · {track.alb || 'No album'}</span><small>{formatTime(track.durationSecs)} · {track.playCount.toLocaleString()} plays</small></>
}

function MergeRecordingSummary({ track, mostPlays }: { track: DecisionTrack; mostPlays: number }) {
  return <><span className="merge-recording-copy"><span className="merge-recording-title"><strong>{track.name}</strong>
    {track.sources.savedAlbums.length > 0 && <span className="merge-badge">Saved album</span>}
    {track.sources.savedTrack && <span className="merge-badge">Saved individually</span>}
    {track.sources.localFile && <span className="merge-badge">Local file</span>}
    {track.sources.retained && <span className="merge-badge">Kept in Retune</span>}
    {!track.sources.membershipKnown && !track.sources.localFile && <span className="merge-badge">Membership unknown</span>}
    {track.playCount > 0 && track.playCount === mostPlays && <span className="merge-badge">Most plays</span>}
  </span><span>{track.art} · {track.alb || 'No album'} · {formatTime(track.durationSecs)}</span></span><span className="merge-recording-plays">{track.playCount.toLocaleString()} plays</span></>
}

type Draft = Omit<TrackMergeEdit, 'rating' | 'playCount'> & { rating: string }
function contributors(preview: TrackMergePreview): DecisionTrack[] {
  return preview.target?.id !== null && preview.target && !preview.tracks.some((track) => track.uri === preview.target?.uri)
    ? [...preview.tracks, preview.target] : preview.tracks
}
function choices(preview: TrackMergePreview) {
  const tracks = contributors(preview)
  return { genres: [...new Set(tracks.map((track) => track.cat).filter((genre) => genre && genre !== 'Uncategorized'))], ratings: [...new Set(tracks.map((track) => track.rating).filter((rating) => rating !== null))] }
}
function initialDraft(preview: TrackMergePreview): Draft {
  const target = preview.target ?? preview.tracks[0]
  const { genres, ratings } = choices(preview)
  return { name: target.name, art: target.art, alb: target.alb, cat: genres.length > 1 ? '' : genres[0] ?? 'Uncategorized', rating: ratings.length > 1 ? '' : ratings[0]?.toString() ?? 'none' }
}

export function TrackMergeDialog({ ids, onClose, onChanged }: { ids: number[]; onClose: () => void; onChanged: (id?: number) => void }) {
  const [preview, setPreview] = useState<TrackMergePreview>()
  const [draft, setDraft] = useState<Draft>()
  const [busy, setBusy] = useState(true)
  const [error, setError] = useState('')
  const [mode, setMode] = useState<MergePlayCount['mode']>('sum')
  const [custom, setCustom] = useState('')
  const [query, setQuery] = useState('')
  const [results, setResults] = useState<SearchTrack[]>([])
  const [nextOffset, setNextOffset] = useState<number | null>(null)
  const [searching, setSearching] = useState(false)
  const [searchError, setSearchError] = useState('')
  const [merged, setMerged] = useState<number>()
  const generation = useRef(0)
  const searchGeneration = useRef(0)
  useEffect(() => {
    const request = ++generation.current
    libraryGateway.mergePreview(ids).then((value) => {
      if (request !== generation.current) return
      setPreview(value); setDraft(initialDraft(value)); setBusy(false)
    }).catch((error) => { if (request === generation.current) { setError(String(error)); setBusy(false) } })
    return () => { beginRequestGeneration(generation); beginRequestGeneration(searchGeneration) }
  }, [ids])
  const reload = async (targetUri?: string) => {
    const request = ++generation.current
    setBusy(true); setError('')
    try {
      const value = await libraryGateway.mergePreview(ids, targetUri)
      if (request !== generation.current) return
      setPreview(value); setDraft(initialDraft(value))
    } catch (error) { if (request === generation.current) setError(String(error)) }
    finally { if (request === generation.current) setBusy(false) }
  }
  const search = async (offset = 0) => {
    if (!query.trim()) return
    const request = ++searchGeneration.current
    setSearching(true); setSearchError('')
    try {
      const result = await spotifyGateway.search(query.trim(), offset)
      if (request !== searchGeneration.current) return
      setResults((previous) => offset ? [...previous, ...result.tracks.items.filter((track) => !previous.some((known) => known.uri === track.uri))] : result.tracks.items)
      setNextOffset(result.tracks.nextOffset)
    } catch (error) { if (request === searchGeneration.current) setSearchError(String(error)) }
    finally { if (request === searchGeneration.current) setSearching(false) }
  }
  const all = preview ? contributors(preview) : []
  const total = mode === 'highest' ? Math.max(0, ...all.map((track) => track.playCount)) : mode === 'sum' ? Math.min(4_294_967_295, all.reduce((sum, track) => sum + track.playCount, 0)) : Number(custom)
  const validCount = mode !== 'custom' || (custom.trim() !== '' && Number.isInteger(total) && total >= 0 && total <= 4_294_967_295)
  const validMetadata = Boolean(draft?.name.trim() && draft.art.trim() && draft.cat.trim() && draft.rating)
  const commit = async () => {
    if (!preview?.target || !draft || !validMetadata || !validCount || busy) return
    setBusy(true); setError('')
    try {
      const id = await libraryGateway.mergeTracks(ids, preview.target.uri, { ...draft, rating: draft.rating === 'none' ? null : Number(draft.rating), playCount: mode === 'custom' ? { mode, value: total } : { mode } }, preview.revision)
      setMerged(id); onChanged(id)
    } catch (error) { setError(String(error)) }
    finally { setBusy(false) }
  }
  const undo = async () => {
    if (merged === undefined) return
    setBusy(true); setError('')
    try { await libraryGateway.undoMerge(merged); onChanged(); onClose() }
    catch (error) { setError(String(error)); setBusy(false) }
  }
  const { genres, ratings } = preview ? choices(preview) : { genres: [], ratings: [] }
  const added = all.flatMap((track) => track.addedAt === null ? [] : [track.addedAt])
  const played = all.flatMap((track) => track.lastPlayedAt === null ? [] : [track.lastPlayedAt])
  const mostPlays = Math.max(0, ...all.map((track) => track.playCount))
  const countExplanation = mode === 'sum' ? `${all.map((track) => track.playCount.toLocaleString()).join(' + ')} summed` : mode === 'highest' ? 'Highest count kept' : 'Custom total'
  const ratingDescription = !draft?.rating ? 'Choose a rating' : draft.rating === 'none' ? 'No track rating override' : `${draft.rating} stars`
  return <ModalDialog className="get-info track-merge" labelledBy="track-merge-title" onCancel={busy ? undefined : onClose} onSubmit={busy ? undefined : merged !== undefined ? onClose : commit}>
    <header className="merge-header"><h2 id="track-merge-title">{merged === undefined ? `Merge ${ids.length} tracks` : 'Tracks merged'}</h2>{merged === undefined && <p>Combine library entries into one recording.</p>}</header>
    <div className="decision-body" aria-busy={busy}>
      {merged !== undefined ? <><p>The selected entries now share one Retune recording. Your Spotify library and local files are unchanged.</p><p>You can also undo this merge later from Get Info.</p></> : !preview || !draft ? <p role="status">{busy ? 'Loading selected tracks…' : 'The selected tracks could not be loaded.'}</p> : <fieldset className="merge-editor" disabled={busy}>
        <section className="merge-section" aria-labelledby="merge-recordings-title">
          <div className="merge-section-heading"><h3 id="merge-recordings-title">Recording Retune plays</h3><p>A saved album first, then the entry with the most plays.</p></div>
          <div className="merge-recordings">{preview.tracks.map((track) => <label key={track.uri} className={`merge-recording ${preview.target?.uri === track.uri ? 'chosen' : ''}`}>
            <input type="radio" name="merge-recording" checked={preview.target?.uri === track.uri} onChange={() => void reload(track.uri)} aria-label={`Use ${track.name} from ${track.alb}`} />
            <MergeRecordingSummary track={track} mostPlays={mostPlays} />
          </label>)}</div>
          {preview.target && !preview.tracks.some((track) => track.uri === preview.target?.uri) && <div className="merge-external-recording"><p>Different recording selected</p><div className="merge-recording chosen"><MergeRecordingSummary track={preview.target} mostPlays={mostPlays} /></div></div>}
          <details><summary>Choose a different Spotify recording</summary><div className="merge-search"><input aria-label="Search recordings" value={query} placeholder="Track and artist" onChange={(event) => { searchGeneration.current++; setQuery(event.target.value); setResults([]); setNextOffset(null); setSearching(false) }} onKeyDown={(event) => { if (event.key === 'Enter') { event.preventDefault(); void search() } }} /><button type="button" disabled={searching || !query.trim()} onClick={() => void search()}>{searching ? 'Searching…' : 'Search'}</button></div>
            {searchError && <p role="alert">{searchError}</p>}
            <div className="merge-search-results">{results.map((track) => <button type="button" key={track.uri} onClick={() => void reload(track.uri)}><strong>{track.name}</strong><span>{track.artist} · {track.alb} · {formatTime(track.durationSecs)}</span></button>)}</div>
            {nextOffset !== null && <button type="button" disabled={searching} onClick={() => void search(nextOffset)}>More results</button>}
          </details>
        </section>
        <section className="merge-section" aria-labelledby="merge-details-title">
          <div className="merge-section-heading"><h3 id="merge-details-title">Surviving details</h3><p>Seeded from “{preview.target?.name ?? preview.tracks[0].name}”. Edit anything here.</p></div>
          <div className="merge-fields">{(['name', 'art', 'alb', 'cat'] as const).map((key) => <label key={key} className={key === 'name' ? 'merge-title-field' : undefined}>{({ name: 'Title', art: 'Artist', alb: 'Album', cat: 'Genre' })[key]}<input maxLength={1024} required={key !== 'alb'} value={draft[key]} list={key === 'cat' ? 'merge-genres' : undefined} onChange={(event) => setDraft({ ...draft, [key]: event.target.value })} /></label>)}
            <label>Rating<select value={draft.rating} onChange={(event) => setDraft({ ...draft, rating: event.target.value })}><option value="" disabled>Choose a rating</option><option value="none">No track override</option>{[1, 2, 3, 4, 5].map((stars) => <option key={stars} value={stars}>{stars} stars</option>)}</select></label>
          </div>
          <datalist id="merge-genres">{genres.map((genre) => <option key={genre}>{genre}</option>)}</datalist>
          {genres.length > 1 && !draft.cat.trim() && <p className="merge-conflict">Genres differ: {genres.join(', ')}. Choose one or enter your own.</p>}
          {ratings.length > 1 && !draft.rating && <p className="merge-conflict">Ratings differ: {ratings.join(', ')} stars. Choose the resulting rating.</p>}
        </section>
        <fieldset className="merge-count"><legend>Play count</legend><p>Retune can’t tell whether these histories overlap.</p>
          <div className="merge-count-options">{(['sum', 'highest', 'custom'] as const).map((option) => <label key={option}><input type="radio" name="play-count" checked={mode === option} onChange={() => setMode(option)} />{{ highest: 'Keep highest', sum: 'Sum (recommended)', custom: 'Set a custom total' }[option]}</label>)}</div>
          {mode === 'custom' && <label className="merge-custom-count">Custom total<input aria-label="Custom play count" type="number" min={0} max={4_294_967_295} step={1} required value={custom} onChange={(event) => setCustom(event.target.value)} /></label>}
        </fieldset>
        <section className="merge-preview" aria-label="Merge preview">
          <div className="merge-review-sources">{all.map((track) => <div key={track.uri}><span title={`${track.name} · ${track.alb}`}>{track.name}</span><strong>{track.playCount.toLocaleString()}</strong></div>)}</div>
          <span className="merge-flow-arrow" aria-hidden="true">→</span>
          <div className="merge-result"><h3>One Retune entry</h3><strong>{draft.name || 'Enter a title'}</strong><span title={`${draft.art} · ${draft.alb}`}>{draft.art} · {draft.alb}</span><div className="merge-total"><output aria-live="polite">{validCount ? total.toLocaleString() : '—'} plays</output><small>{countExplanation}</small></div><small>{draft.cat || 'Choose a genre'} · {ratingDescription} · first added {date(added.length ? Math.min(...added) : null)} · last played {date(played.length ? Math.max(...played) : null)}</small></div>
        </section>
        {preview.target && !preview.tracks.some((track) => track.uri === preview.target?.uri) && <p className="merge-conflict">You chose a different recording. Playback will use “{preview.target.name}” ({formatTime(preview.target.durationSecs)}).{preview.target.id !== null && ' Its existing history is included above.'}</p>}
      </fieldset>}
      {error && <div className="decision-error" role="alert">{error}{merged === undefined && <button type="button" disabled={busy} onClick={() => void reload(preview?.target?.uri)}>Reload preview</button>}</div>}
    </div>
    <footer className="modal-actions">{merged !== undefined ? <><button type="button" disabled={busy} onClick={() => void undo()}>Undo merge</button><button type="submit" className="primary" disabled={busy}>Done</button></> : <><p>Merge changes only Retune. Undo restores the originals and keeps later edits and plays.</p><button type="button" disabled={busy} onClick={onClose}>Cancel</button><button type="submit" className="primary" disabled={busy || !preview?.target || !validMetadata || !validCount}>{busy ? 'Working…' : 'Merge tracks'}</button></>}</footer>
  </ModalDialog>
}

export function RemoveTrackDialog({ tracks, spotify, onClose, onChanged }: { tracks: Pick<Track, 'id' | 'name' | 'uri'>[]; spotify: boolean; onClose: () => void; onChanged: () => void }) {
  const [info, setInfo] = useState<TrackInfo>()
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  useEffect(() => {
    if (!spotify) return
    let active = true
    libraryGateway.getTrack(tracks[0].id).then((value) => { if (active) setInfo(value) }).catch((error) => { if (active) setError(String(error)) })
    return () => { active = false }
  }, [spotify, tracks])
  const canRemove = !spotify || Boolean(info?.sources.savedTrack && info.sources.membershipKnown)
  const remove = async () => {
    if (!canRemove || busy) return
    setBusy(true); setError('')
    try {
      if (spotify) await spotifyGateway.removeTrack(tracks[0].uri)
      else await libraryGateway.removeTracks(tracks.map((track) => track.id))
      onChanged(); onClose()
    } catch (error) { setError(String(error)); setBusy(false) }
  }
  const exclude = async () => {
    setBusy(true); setError('')
    try { await libraryGateway.setTrackEnabled(tracks[0].id, false); onChanged(); onClose() }
    catch (error) { setError(String(error)); setBusy(false) }
  }
  const referenced = info && (info.sources.savedAlbums.length > 0 || info.mergedSources.some((track) => track.uri !== info.uri && (track.sources.savedTrack || track.sources.savedAlbums.length > 0)))
  return <ModalDialog className="get-info track-removal" labelledBy="track-removal-title" onCancel={busy ? undefined : onClose} onSubmit={remove}>
    <h2 id="track-removal-title">{spotify ? 'Remove from Spotify?' : `Remove ${tracks.length === 1 ? 'track' : `${tracks.length} tracks`} from Retune?`}</h2>
    <p>{tracks.length === 1 ? tracks[0].name : `${tracks.length} selected tracks`}</p>
    {spotify ? info ? <><SourcesList sources={info.sources} />{!info.sources.membershipKnown ? <p>Sync Spotify to check membership before removing this track.</p> : !info.sources.savedTrack ? <p>This recording is not individually saved in Spotify.{info.sources.savedAlbums.length ? ' It comes from a saved album. You can exclude it from sequential playback without removing the album.' : ''}</p> : <p>{referenced ? 'Unsave this individual recording in Spotify. Other saved sources still supply the Retune entry, so it will remain in the library and be excluded from sequential playback.' : 'Unsave this individual recording in Spotify and move its Retune entry to Removed tracks. Its metadata and history are preserved.'}</p>}{info.mergedSources.length > 0 && <p>This action applies to the chosen recording only. Other merged Spotify recordings keep their current saved status.</p>}</> : <p role="status">Checking library sources…</p> : <p>Hide {tracks.length === 1 ? 'this entry' : 'these entries'} from Retune and keep metadata and play history. Sync will keep them hidden. Restore them from Removed tracks. Spotify and local files stay unchanged.</p>}
    {error && <p role="alert" className="decision-error">{error}</p>}
    <div className="modal-actions"><button type="button" disabled={busy} onClick={onClose}>Cancel</button>{spotify && info && !canRemove && info.enabled && <button type="button" disabled={busy} onClick={() => void exclude()}>Exclude from playback</button>}<button type="submit" className={spotify ? 'danger' : 'primary'} disabled={busy || !canRemove}>{busy ? 'Removing…' : spotify ? 'Remove from Spotify' : 'Remove from Retune'}</button></div>
  </ModalDialog>
}

export function RemovedTracksDialog({ onClose, onChanged }: { onClose: () => void; onChanged: () => void }) {
  const [tracks, setTracks] = useState<DecisionTrack[]>()
  const [busy, setBusy] = useState<string>()
  const [error, setError] = useState('')
  const [query, setQuery] = useState('')
  useEffect(() => {
    let active = true
    libraryGateway.removedTracks().then((value) => { if (active) setTracks(value) }).catch((error) => { if (active) setError(String(error)) })
    return () => { active = false }
  }, [])
  const restore = async (uri: string) => {
    setBusy(uri); setError('')
    try { await libraryGateway.restoreTrack(uri); setTracks((tracks) => tracks?.filter((track) => track.uri !== uri)); onChanged() }
    catch (error) { setError(String(error)) }
    finally { setBusy(undefined) }
  }
  return <ModalDialog className="get-info removed-tracks" labelledBy="removed-tracks-title" onCancel={busy ? undefined : onClose}>
    <h2 id="removed-tracks-title">Removed tracks</h2><p>Restore entries with their metadata and history. Restoring here does not save anything in Spotify.</p>
    <input aria-label="Filter removed tracks" placeholder="Filter by track, artist or album" value={query} onChange={(event) => setQuery(event.target.value)} />
    <div className="decision-body">{tracks ? tracks.length ? tracks.filter((track) => `${track.name} ${track.art} ${track.alb}`.toLowerCase().includes(query.toLowerCase())).map((track) => <div key={track.uri} className="removed-track"><span className="decision-track"><TrackSummary track={track} /></span><button type="button" disabled={busy !== undefined} onClick={() => void restore(track.uri)}>{busy === track.uri ? 'Restoring…' : 'Add to Retune'}</button></div>) : <p>No removed tracks.</p> : <p role="status">{error ? 'Could not load removed tracks.' : 'Loading removed tracks…'}</p>}{error && <p role="alert" className="decision-error">{error}</p>}</div>
    <div className="modal-actions"><button type="button" disabled={busy !== undefined} onClick={onClose}>Done</button></div>
  </ModalDialog>
}
