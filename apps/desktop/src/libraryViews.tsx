import { useEffect, useRef, useState } from 'react'
import type { BrowseView, ColumnKey, Playing, PlaylistSubject, Selection, Settings, Source, Track } from './types.ts'
import { COLUMN_SPECS, DRAG_LOCAL_TYPE, DRAG_TYPE, facetLabel, formatTime, hasLocalTracks, isCurrentTrack, labels, moveBefore, resizedColumnWidth, resizedPaneHeight, trackColumnHeadings, trackGridColumns, visibleColumnOrder } from './ui.ts'
import { CheckboxMenu, ContextMenu, RatingStars } from './viewShared.tsx'

export function BrowserPane({ state, anchors, onActivate, onSelect, onPlay, onToggle }: {
  state: { source: Source; view: BrowseView | null; settings: Pick<Settings, 'browserPanes' | 'browserVisible'>; sel: Selection }
  anchors: { current: Partial<Record<keyof Selection, string>> }
  onActivate: (facet: keyof Selection) => void
  onSelect: (facet: keyof Selection, values: string[], anchor?: string) => void
  onPlay: (facet: keyof Selection, values: string[], anchor?: string) => void
  onToggle: (facet: keyof Selection) => void
}) {
  const [menu, setMenu] = useState<{ x: number; y: number }>()
  const [height, setHeight] = useState(200)
  const pane = useRef<HTMLDivElement>(null)
  const resize = useRef<{ pointerId: number; startY: number; startHeight: number; maxHeight: number; zoom: number } | undefined>(undefined)
  const sourceLabels = labels[state.source].facets
  const values = [state.view?.facets.cats ?? [], state.view?.facets.arts ?? [], state.view?.facets.albs ?? []]
  const facets: (keyof Selection)[] = ['cat', 'art', 'alb']
  const visible = facets.filter((facet) => state.settings.browserPanes[facet])
  if (!state.settings.browserVisible || !visible.length) return null
  const maxHeight = () => Math.max(90, (pane.current?.parentElement?.clientHeight ?? 340) - 140)
  const adjustHeight = (delta: number) => setHeight((current) => resizedPaneHeight(current, 0, delta, maxHeight(), 1))
  return <div ref={pane} className="browser-pane" style={{ height, flexBasis: height }}>
    <div className="browser-scroll"><div className="browser-columns" style={{ minWidth: `${visible.length * 200}px`, gridTemplateColumns: `repeat(${visible.length}, minmax(200px, 1fr))` }}>
      {facets.map((facet, index) => state.settings.browserPanes[facet] && <FacetColumn key={facet} facet={facet} title={sourceLabels[index]} values={values[index]} selected={state.sel[facet]} anchor={anchors.current[facet]} onActivate={() => onActivate(facet)} onSelect={(selected, anchor) => onSelect(facet, selected, anchor)} onPlay={(selected, anchor) => onPlay(facet, selected, anchor)} onContextMenu={(event) => {
        event.preventDefault()
        setMenu({ x: event.clientX, y: event.clientY })
      }} />)}
    </div></div>
    {menu && <CheckboxMenu x={menu.x} y={menu.y} onClose={() => setMenu(undefined)} items={facets.map((facet, index) => ({ key: facet, label: sourceLabels[index], checked: state.settings.browserPanes[facet], onChange: () => onToggle(facet) }))} />}
    <span className="browser-resize-handle" role="separator" aria-label="Resize column browser" aria-orientation="horizontal" tabIndex={0} onPointerDown={(event) => {
      if (event.button !== 0 || !pane.current) return
      event.preventDefault()
      event.currentTarget.setPointerCapture(event.pointerId)
      resize.current = {
        pointerId: event.pointerId,
        startY: event.clientY,
        startHeight: Number.parseFloat(getComputedStyle(pane.current).height),
        maxHeight: maxHeight(),
        zoom: Number(pane.current.closest<HTMLElement>('.app-shell')?.style.zoom) || 1,
      }
    }} onPointerMove={(event) => {
      const active = resize.current
      if (!active || active.pointerId !== event.pointerId) return
      setHeight(resizedPaneHeight(active.startHeight, active.startY, event.clientY, active.maxHeight, active.zoom))
    }} onPointerUp={(event) => {
      if (resize.current?.pointerId === event.pointerId) resize.current = undefined
    }} onPointerCancel={() => { resize.current = undefined }} onKeyDown={(event) => {
      if (event.key === 'ArrowUp') { event.preventDefault(); adjustHeight(-16) }
      if (event.key === 'ArrowDown') { event.preventDefault(); adjustHeight(16) }
    }} />
  </div>
}

function FacetColumn({ facet, title, values, selected, anchor, onActivate, onSelect, onPlay, onContextMenu }: {
  facet: keyof Selection
  title: string
  values: string[]
  selected?: string[]
  anchor?: string
  onActivate: () => void
  onSelect: (values: string[], anchor?: string) => void
  onPlay: (values: string[], anchor?: string) => void
  onContextMenu: (event: React.MouseEvent) => void
}) {
  const select = (value: string, event: React.MouseEvent) => {
    if (event.shiftKey) {
      const start = Math.max(0, values.indexOf(anchor ?? values[0]))
      const end = values.indexOf(value)
      onSelect(values.slice(Math.min(start, end), Math.max(start, end) + 1), anchor)
    } else if (event.metaKey || event.ctrlKey) {
      onSelect(selected?.includes(value) ? selected.filter((item) => item !== value) : [...(selected ?? []), value], value)
    } else {
      onSelect([value], value)
    }
  }
  return <div className="facet-column" data-facet={facet} onMouseDown={onActivate} onContextMenu={onContextMenu}>
    <div className="column-header">{title}</div>
    <div className="facet-list">
      <button data-row-index={0} className={!selected?.length ? 'active' : ''} onClick={() => onSelect([], undefined)} onDoubleClick={() => onPlay([], undefined)}>All ({values.length} {title}s)</button>
      {values.map((value, index) => {
        const label = facetLabel(title, value)
        const meta = label !== value
        return <button key={value} data-row-index={index + 1} className={`${selected?.includes(value) ? 'active' : ''} ${meta ? 'meta' : ''}`} onClick={(event) => select(value, event)} onDoubleClick={() => onPlay([value], value)} title={meta ? 'Tracks without genre metadata' : value}>{label}</button>
      })}
    </div>
  </div>
}

export function AlbumRatingStrip({ album, rating, onRate }: { album: string; rating: number | null; onRate: (rating: number | null) => void }) {
  return <div className="album-rating-strip"><strong>{album}</strong><RatingStars rating={rating} explicit onRate={(stars) => onRate(stars === rating ? null : stars)} /></div>
}

type TrackCellData = Omit<Track, 'id'> & { id: number | null }

export function TrackCell({ track, column, playing, selected, onInfo, onRate }: {
  track: TrackCellData
  column: ColumnKey
  playing: false | 'playing' | 'paused'
  selected: boolean
  onInfo?: () => void
  onRate?: (stars: number) => void
}) {
  if (column === 'disc') return <span className="track-number">{track.discNo ?? 1}</span>
  if (column === 'track') return <span className="track-number">{track.trackNo ?? ''}</span>
  if (column === 'name') return <span className="track-name" title={track.name}>{playing && <span className="playing-marker">{playing === 'playing' ? '▶' : '❚❚'}</span>}{track.isLocal && <span className="local-glyph" aria-label="Local file">⌂</span>}<span className="track-title">{track.name}</span>{selected && onInfo && <button className="info-button" aria-label={`Get info for ${track.name}`} onClick={(event) => { event.stopPropagation(); onInfo() }}>ⓘ</button>}</span>
  if (column === 'time') return <span className="track-number">{formatTime(track.durationSecs)}</span>
  if (column === 'artist') return <span title={track.art}>{track.art}</span>
  if (column === 'album') return <span title={track.alb}>{track.alb}</span>
  if (column === 'genre') return <span title={track.cat}>{track.cat}</span>
  if (column === 'plays') return <span className="track-number">{track.playCount || ''}</span>
  if (column === 'kind') return <span title={track.kind ?? undefined}>{track.kind ?? ''}</span>
  if (column === 'bitrate') return <span className="track-number">{track.bitrateKbps === null ? '' : `${track.bitrateKbps} kbps`}</span>
  if (column === 'lastPlayed') return <span className="track-number">{track.lastPlayedAt === null ? '' : new Date(track.lastPlayedAt * 1000).toLocaleString(undefined, { dateStyle: 'short', timeStyle: 'short' })}</span>
  if (column === 'added') return <span className="track-number">{track.addedAt === null ? '' : new Date(track.addedAt * 1000).toLocaleDateString()}</span>
  if (column === 'releaseDate') return <span className="track-number">{track.releaseDate ?? ''}</span>
  return <RatingStars rating={track.rating?.stars ?? null} explicit={track.rating?.explicit} onRate={onRate} />
}

export function TrackList({ tracks, label, selectedIds, playing, columnOrder, columnWidths, hiddenColumns, sortColumn, sortDesc, empty, onActivate, onSetup, onSelect, onClearSelection, onPlay, onEnabled, onRate, onInfo, onPlaylist, onGoToAlbum, onGoToArtist, onReorder, onColumnWidths, onHiddenColumns, onSort }: {
  tracks: Track[]; label: (typeof labels)[Source]; selectedIds: Set<number>; playing: Playing | null
  columnOrder: ColumnKey[]; columnWidths: Partial<Record<ColumnKey, number>>; hiddenColumns: ColumnKey[]; sortColumn: ColumnKey | null; sortDesc: boolean; empty: boolean; onSelect: (id: number, event: React.MouseEvent) => void; onPlay: (id: number) => void; onEnabled: (id: number, enabled: boolean) => void
  onRate: (id: number, stars: number) => void; onInfo: (id: number) => void; onReorder: (order: ColumnKey[]) => void
  onColumnWidths: (widths: Partial<Record<ColumnKey, number>>) => void
  onPlaylist: (subject: PlaylistSubject) => void
  onGoToAlbum: (track: Track) => void; onGoToArtist: (track: Track) => void
  onActivate: () => void; onSetup: () => void; onClearSelection: () => void; onHiddenColumns: (columns: ColumnKey[]) => void; onSort: (column: ColumnKey, desc: boolean) => void
}) {
  const [liveWidths, setLiveWidths] = useState(columnWidths)
  const [menu, setMenu] = useState<{ x: number; y: number; trackId?: number }>()
  const headerDragged = useRef(false)
  const columnDrag = useRef<{ column: ColumnKey; pointerId: number; startX: number; element: HTMLSpanElement } | undefined>(undefined)
  const resize = useRef<{ column: ColumnKey; pointerId: number; startX: number; startWidth: number } | undefined>(undefined)
  useEffect(() => setLiveWidths(columnWidths), [columnWidths])
  const headings = trackColumnHeadings(label)
  const visibleColumns = visibleColumnOrder(columnOrder, hiddenColumns)
  const columns = trackGridColumns(visibleColumns, liveWidths)
  const moveColumn = (event: React.PointerEvent<HTMLSpanElement>) => {
    const active = columnDrag.current
    if (!active || active.pointerId !== event.pointerId) return
    if (Math.abs(event.clientX - active.startX) > 4) {
      headerDragged.current = true
      active.element.classList.add('dragging')
    }
  }
  const endColumn = (event: React.PointerEvent<HTMLSpanElement>) => {
    const active = columnDrag.current
    if (!active || active.pointerId !== event.pointerId) return
    active.element.classList.remove('dragging')
    columnDrag.current = undefined
    if (!headerDragged.current) return
    const target = visibleColumns.find((column) => {
      const element = event.currentTarget.parentElement?.querySelector<HTMLElement>(`[data-column="${column}"]`)
      return element != null && event.clientX < element.getBoundingClientRect().left + element.getBoundingClientRect().width / 2
    })
    onReorder(moveBefore(columnOrder, active.column, target))
  }
  const beginResize = (event: React.PointerEvent<HTMLSpanElement>, column: ColumnKey) => {
    event.preventDefault()
    event.stopPropagation()
    headerDragged.current = true
    event.currentTarget.setPointerCapture(event.pointerId)
    resize.current = { column, pointerId: event.pointerId, startX: event.clientX, startWidth: event.currentTarget.parentElement?.getBoundingClientRect().width ?? 60 }
  }
  const moveResize = (event: React.PointerEvent<HTMLSpanElement>) => {
    const active = resize.current
    if (!active || active.pointerId !== event.pointerId) return
    setLiveWidths((widths) => ({ ...widths, [active.column]: resizedColumnWidth(active.startWidth, active.startX, event.clientX) }))
  }
  const endResize = (event: React.PointerEvent<HTMLSpanElement>) => {
    const active = resize.current
    if (!active || active.pointerId !== event.pointerId) return
    const width = resizedColumnWidth(active.startWidth, active.startX, event.clientX)
    resize.current = undefined
    onColumnWidths({ ...columnWidths, [active.column]: width })
  }
  const cancelResize = (event: React.PointerEvent<HTMLSpanElement>) => {
    if (resize.current?.pointerId !== event.pointerId) return
    resize.current = undefined
    setLiveWidths(columnWidths)
  }
  const menuTrack = menu?.trackId === undefined ? undefined : tracks.find((track) => track.id === menu.trackId)
  return <div className="track-list" onMouseDown={onActivate}>
    <div className={`track-scroll ${empty ? 'empty-library' : ''}`} onClick={(event) => { if (event.target === event.currentTarget) onClearSelection() }}><div className="track-row track-header" style={{ gridTemplateColumns: columns }} onContextMenu={(event) => {
      event.preventDefault()
      setMenu({ x: event.clientX, y: event.clientY })
    }}><span className="track-enabled-cell" />{visibleColumns.map((column) => <span key={column} data-column={column} className={COLUMN_SPECS[column].numeric ? 'track-number' : ''} onPointerDown={(event) => {
      if (event.button !== 0) return
      headerDragged.current = false
      columnDrag.current = { column, pointerId: event.pointerId, startX: event.clientX, element: event.currentTarget }
      event.currentTarget.setPointerCapture(event.pointerId)
    }} onPointerMove={moveColumn} onPointerUp={endColumn} onPointerCancel={() => {
      columnDrag.current?.element.classList.remove('dragging')
      columnDrag.current = undefined
    }} onClick={() => {
      if (headerDragged.current) return
      onSort(column, sortColumn === column ? !sortDesc : false)
    }}><span className="track-header-label">{headings[column]}{sortColumn === column ? sortDesc ? ' ▼' : ' ▲' : ''}</span><span className="column-resize-handle" draggable={false} onPointerDown={(event) => beginResize(event, column)} onPointerMove={moveResize} onPointerUp={endResize} onPointerCancel={cancelResize} onClick={(event) => {
      event.preventDefault()
      event.stopPropagation()
    }} onDragStart={(event) => {
      event.preventDefault()
      event.stopPropagation()
    }} /></span>)}</div>
      {empty ? <div className="empty-prompt"><span className="empty-glyph" aria-hidden="true">♪</span><strong>Your library is empty</strong><span>Connect Spotify and sync to pull your saved music into a local overlay.</span><button onClick={onSetup}>Set Up Library…</button></div> : tracks.map((track) => {
        const isPlaying = isCurrentTrack(playing, track)
        return <div key={track.id} data-track-id={track.id} draggable className={`track-row ${selectedIds.has(track.id) ? 'selected' : ''} ${isPlaying ? 'playing' : ''}`} style={{ gridTemplateColumns: columns }} onClick={(event) => onSelect(track.id, event)} onDoubleClick={() => onPlay(track.id)} onDragStart={(event) => {
          const dragged = selectedIds.has(track.id) ? tracks.filter((candidate) => selectedIds.has(candidate.id)) : [track]
          event.dataTransfer.effectAllowed = 'copy'
          const subject = { kind: 'tracks', label: dragged.length === 1 ? `Track · ${dragged[0].name}` : `${dragged.length} tracks`, uris: dragged.map((candidate) => candidate.uri) } satisfies PlaylistSubject
          event.dataTransfer.setData(DRAG_TYPE, JSON.stringify(subject))
          if (hasLocalTracks(subject)) event.dataTransfer.setData(DRAG_LOCAL_TYPE, '')
        }} onContextMenu={(event) => {
          event.preventDefault()
          if (!selectedIds.has(track.id)) onSelect(track.id, event)
          setMenu({ x: event.clientX, y: event.clientY, trackId: track.id })
        }}>
          <span className="track-enabled-cell"><input type="checkbox" checked={track.enabled} aria-label={`${track.enabled ? 'Exclude' : 'Include'} ${track.name} from sequential playback`} title={track.enabled ? 'Uncheck to skip during sequential playback' : 'Check to include in sequential playback'} onClick={(event) => event.stopPropagation()} onDoubleClick={(event) => event.stopPropagation()} onChange={(event) => onEnabled(track.id, event.target.checked)} /></span>
          {visibleColumns.map((column) => <TrackCell key={column} track={track} column={column} playing={isPlaying ? playing?.isPlaying ? 'playing' : 'paused' : false} selected={selectedIds.has(track.id)} onInfo={() => onInfo(track.id)} onRate={(stars) => onRate(track.id, stars)} />)}
        </div>
      })}
    </div>
    {menu && (menu.trackId === undefined
      ? <CheckboxMenu x={menu.x} y={menu.y} onClose={() => setMenu(undefined)} items={columnOrder.map((column) => ({
        key: column,
        label: headings[column],
        checked: !hiddenColumns.includes(column),
        disabled: column === 'name',
        onChange: (checked) => onHiddenColumns(checked ? hiddenColumns.filter((hidden) => hidden !== column) : [...hiddenColumns, column]),
      }))} />
      : <ContextMenu x={menu.x} y={menu.y} onClose={() => setMenu(undefined)}>
        <button onClick={() => {
          const target = tracks.find((track) => track.id === menu.trackId)
          if (!target) return
          const selected = selectedIds.has(target.id) ? tracks.filter((track) => selectedIds.has(track.id)) : [target]
          setMenu(undefined)
          onPlaylist({ kind: 'tracks', label: selected.length === 1 ? `Track · ${selected[0].name}` : `${selected.length} tracks`, uris: selected.map((track) => track.uri) })
        }}>Add to Playlist…</button>
        <button disabled={!menuTrack || menuTrack.isLocal} onClick={() => { setMenu(undefined); if (menuTrack) onGoToAlbum(menuTrack) }}>View album in Spotify</button>
        <button disabled={!menuTrack || menuTrack.isLocal} onClick={() => { setMenu(undefined); if (menuTrack) onGoToArtist(menuTrack) }}>View artist albums in Spotify</button>
        <button onClick={() => { const id = menu.trackId; setMenu(undefined); if (id !== undefined) onInfo(id) }}>Get Info</button>
      </ContextMenu>)}
  </div>
}
