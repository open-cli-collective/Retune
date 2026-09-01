import { useCallback, useEffect, useEffectEvent, useRef, useState, type ReactNode } from 'react'
import type { AlbumPageView, ArtistAlbumsPage, ArtistPageView, PlaybackTrack, PlaylistSubject, SearchAlbum, SearchArtist, SearchTrack, SpotifyNavEntry, SpotifyResults } from './types.ts'
import { createSpotifySearchState, expandSpotifySearchGroup, failSpotifySearchGroup, moreSpotifySearchLabel, receiveSpotifySearchPage, replaceSpotifySearchResults, resetSpotifySearchQuery, retrySpotifySearchGroup, setSpotifySearchTab, spotifyMembership, spotifySearchGroupHeader, spotifySearchPendingPageKey, type SpotifyMembershipOverrides, type SpotifySearchState } from './spotifySearch.ts'
import { beginPendingEntity, DRAG_TYPE, entityRequestGeneration, formatTime, loadCurrentGeneration, mergeByUri, pendingEntities, SYNTHETIC_BASE } from './ui.ts'
import { ContextMenu, RatingStars } from './viewShared.tsx'
import { libraryGateway } from './libraryGateway.ts'
import { spotifyGateway } from './spotifyGateway.ts'

type SpotifyTab = 'all' | keyof SpotifyResults

function SpotifyArtwork({ imageUrl, round = false }: { imageUrl: string | null; round?: boolean }) {
  return <span className={`spotify-artwork ${round ? 'round' : ''}`}>{imageUrl ? <img src={imageUrl} alt="" /> : <span aria-hidden="true">♪</span>}</span>
}

function SpotifyAlbumRow({ album, adding, added, onAdd, onRemove, onOpen, onPlaylist, showType = false, searchActions = false, onPlay, playing = false }: {
  album: SearchAlbum
  adding: boolean
  added: boolean
  onAdd: () => void
  onRemove: () => void
  onOpen: () => void
  onPlaylist: (subject: PlaylistSubject) => void
  showType?: boolean
  searchActions?: boolean
  onPlay?: () => void
  playing?: boolean
}) {
  const [menu, setMenu] = useState<{ x: number; y: number }>()
  const subject: PlaylistSubject = { kind: 'album', label: `Album · ${album.name}`, albumUri: album.uri }
  return <div className={`spotify-row${searchActions ? ' spotify-search-row' : ''}`} aria-current={playing ? 'true' : undefined} draggable onDragStart={(event) => {
    event.dataTransfer.effectAllowed = 'copy'
    event.dataTransfer.setData(DRAG_TYPE, JSON.stringify(subject))
  }} onContextMenu={(event) => { event.preventDefault(); setMenu({ x: event.clientX, y: event.clientY }) }}>
    <SpotifyArtwork imageUrl={album.imageUrl} />
    <button type="button" className="spotify-copy spotify-copy-button" onClick={onOpen}><strong>{album.name}</strong><small>{showType ? [album.year, album.albumType].filter(Boolean).join(' · ') : <>{album.artist}{album.year && ` · ${album.year}`}</>}</small></button>
    {searchActions ? <SpotifyResultActions name={album.name} onPlay={onPlay} playing={playing} adding={adding} added={added} onAdd={onAdd} onRemove={onRemove} onOpen={onOpen} />
      : <button className="spotify-add" disabled={adding} title={added ? 'Remove from Library' : 'Add to Library'} aria-label={added ? `Remove ${album.name} from Library` : `Add ${album.name} to Library`} onClick={(event) => { event.stopPropagation(); if (added) onRemove(); else onAdd() }} onDoubleClick={(event) => event.stopPropagation()}>{adding ? added ? 'Removing…' : 'Adding…' : added ? '✓ In Library' : '+ Add'}</button>}
    {menu && <ContextMenu x={menu.x} y={menu.y} onClose={() => setMenu(undefined)}><button onClick={() => { setMenu(undefined); onPlaylist(subject) }}>Add to Playlist…</button></ContextMenu>}
  </div>
}

function SpotifyResultActions({ name, meta = '', onPlay, playing = false, adding = false, added = false, onAdd, onRemove, onOpen }: {
  name: string
  meta?: string
  onPlay?: () => void
  playing?: boolean
  adding?: boolean
  added?: boolean
  onAdd?: () => void
  onRemove?: () => void
  onOpen?: () => void
}) {
  return <span className="spotify-result-actions">
    <span className="spotify-result-meta">{meta}</span>
    {onPlay ? <button className="spotify-play-round" type="button" disabled={adding} title={`Play ${name}`} aria-label={`Play ${name}`} aria-pressed={playing} onClick={(event) => { event.stopPropagation(); onPlay() }} onDoubleClick={(event) => event.stopPropagation()}>▶</button> : <span className="spotify-result-empty" />}
    {onAdd && onRemove ? <button className="spotify-add" type="button" disabled={adding} title={added ? `Remove ${name} from Library` : `Add ${name} to Library`} aria-label={added ? `Remove ${name} from Library` : `Add ${name} to Library`} onClick={(event) => { event.stopPropagation(); if (added) onRemove(); else onAdd() }} onDoubleClick={(event) => event.stopPropagation()}>{adding ? added ? 'Removing…' : 'Adding…' : added ? '✓ Added' : '+ Add'}</button> : <span className="spotify-result-empty" />}
    <button className="spotify-result-view" type="button" disabled={!onOpen} title={`View ${name}`} aria-label={`View ${name}`} onClick={(event) => { event.stopPropagation(); onOpen?.() }} onDoubleClick={(event) => event.stopPropagation()}><span aria-hidden="true">›</span></button>
  </span>
}

function SpotifyArtistRow({ artist, onOpen }: { artist: SearchArtist; onOpen: () => void }) {
  return <div className="spotify-row spotify-search-row">
    <SpotifyArtwork imageUrl={artist.imageUrl} round />
    <button type="button" className="spotify-copy spotify-copy-button" onClick={onOpen}><strong>{artist.name}</strong><small>{artist.descriptor}</small></button>
    <SpotifyResultActions name={artist.name} onOpen={onOpen} />
  </div>
}

function albumPlaybackTracks(page: AlbumPageView): PlaybackTrack[] {
  return page.tracks.map((track, index) => ({
    id: track.trackId ?? SYNTHETIC_BASE + index,
    uri: track.uri,
    name: track.name,
    art: page.artist,
    alb: page.name,
    durationSecs: track.durationSecs,
    enabled: track.enabled,
  }))
}

function searchTrackPlayback(track: SearchTrack, index: number): PlaybackTrack {
  return { id: SYNTHETIC_BASE + index, uri: track.uri, name: track.name, art: track.artist, alb: track.alb, durationSecs: track.durationSecs, enabled: true }
}

function SpotifySearchSection({ group, state, onMore, onRetry, children }: { group: keyof SpotifyResults; state: SpotifySearchState; onMore: () => void; onRetry: () => void; children: ReactNode }) {
  const resultGroup = state.groups[group]
  const loading = state.loading.has(group)
  const error = state.errors[group]
  const more = moreSpotifySearchLabel(state, group)
  return <section aria-busy={loading}>
    <h2>{spotifySearchGroupHeader(state, group)}</h2>
    {children}
    {!resultGroup.items.length && !loading && !error && <p>No {group} found.</p>}
    {loading && <p className="spotify-search-loading">Loading {group}…</p>}
    {error && <div className="spotify-search-error"><span>{error}</span><button type="button" onClick={onRetry}>Retry</button></div>}
    {!loading && !error && more && <div className="spotify-search-more"><button type="button" onClick={onMore}>{more}</button></div>}
  </section>
}

export function SpotifyPageBack({ label, onBack }: { label: string; onBack: () => void }) {
  return <button className="spotify-page-back" onClick={onBack}>‹ Back to {label}</button>
}

export type SpotifyAlbumPresentationData = {
  uri: string
  name: string
  artist: string
  albumType?: string | null
  year?: string | null
  imageUrl?: string | null
  tracks: { uri: string; name: string; trackNo?: number | null; durationSecs?: number; matchState?: 'matched' | 'ambiguous' | 'unmatched' }[]
}

/** Album artwork, metadata, and track rows without playback or membership controls. */
export function SpotifyAlbumPresentation({ album, compact = false, titleAs = 'h2', artistContent, headerMeta, headerActions, renderTrack, trackFooter }: {
  album: SpotifyAlbumPresentationData
  compact?: boolean
  titleAs?: 'h1' | 'h2'
  artistContent?: ReactNode
  headerMeta?: ReactNode
  headerActions?: ReactNode
  renderTrack?: (track: SpotifyAlbumPresentationData['tracks'][number], index: number) => ReactNode
  trackFooter?: ReactNode
}) {
  const totalDurationSecs = album.tracks.reduce((total, track) => total + (track.durationSecs ?? 0), 0)
  const title = titleAs === 'h1' ? <h1>{album.name}</h1> : <h2>{album.name}</h2>
  return <section className={`spotify-album-presentation${compact ? ' compact' : ''}`} aria-label={`${album.name} album preview`}>
    <header className="spotify-page-header album-header">
      <div className="spotify-page-art album-art"><SpotifyArtwork imageUrl={album.imageUrl ?? null} /></div>
      <div className="spotify-page-copy">
        <div className="spotify-eyebrow">ALBUM{album.albumType && album.albumType.toLowerCase() !== 'album' && ` · ${album.albumType.toUpperCase()}`}</div>
        {title}
        {artistContent ?? <p className="spotify-link artist-link">{album.artist}</p>}
        {headerMeta ?? <div className="spotify-page-meta"><span>{album.year && `${album.year} · `}{album.tracks.length} {album.tracks.length === 1 ? 'track' : 'tracks'} · {Math.floor(totalDurationSecs / 60)} min</span></div>}
        {headerActions}
      </div>
    </header>
    <div className="spotify-page-section album-tracks">
      {album.tracks.map((track, index) => renderTrack?.(track, index) ?? <div className="spotify-track-row" key={`${track.uri}-${index}`}><span>{track.trackNo ?? index + 1}</span><span>{track.name}{track.matchState && <small className={`spotify-track-match-state ${track.matchState}`}>{track.matchState}</small>}</span><time>{formatTime(track.durationSecs ?? 0)}</time></div>)}
      {trackFooter}
    </div>
  </section>
}

function SpotifyAlbumPage({ entry, backLabel, adding, membership, playingUri, onBack, onArtist, onAdd, onRemove, onAddTrack, onRemoveTrack, onPlay, onPlaylist, onError }: {
  entry: Extract<SpotifyNavEntry, { kind: 'album' }>
  backLabel: string
  adding: boolean
  membership: SpotifyMembershipOverrides
  playingUri: string | null
  onBack: () => void
  onArtist: (id: string) => void
  onAdd: (album: { uri: string; name: string; artist: string }) => Promise<boolean>
  onRemove: (uri: string) => Promise<boolean>
  onAddTrack: (uri: string) => Promise<boolean>
  onRemoveTrack: (uri: string) => Promise<boolean>
  onPlay: (id: number, tracks: readonly PlaybackTrack[]) => void
  onPlaylist: (subject: PlaylistSubject) => void
  onError: (error: string) => void
}) {
  const [loaded, setLoaded] = useState<{ uri: string; page?: AlbumPageView }>({ uri: entry.uri })
  const [revision, setRevision] = useState(0)
  const [busy, setBusy] = useState(false)
  const [trackBusy, setTrackBusy] = useState<Set<string>>(new Set())
  const pendingTracks = useRef(new Set<string>())
  const [menu, setMenu] = useState<{ x: number; y: number; index: number }>()
  const highlighted = useRef<HTMLDivElement>(null)
  const ratingGenerations = useRef(new Map<string, { current: number }>())
  const reportError = useEffectEvent(onError)
  useEffect(() => {
    pendingTracks.current.clear()
    setTrackBusy(new Set())
  }, [entry.uri])
  useEffect(() => {
    let active = true
    setLoaded({ uri: entry.uri })
    spotifyGateway.albumPage(entry.uri)
      .then((view) => active && setLoaded({ uri: entry.uri, page: view }))
      .catch((error) => active && reportError(String(error)))
    return () => { active = false }
  }, [entry.uri, revision])
  const page = loaded.uri === entry.uri ? loaded.page : undefined
  useEffect(() => {
    if (page && entry.highlight) highlighted.current?.scrollIntoView({ block: 'center' })
  }, [entry.highlight, page])
  if (!page) return <div className="spotify-page"><SpotifyPageBack label={backLabel} onBack={onBack} /><div className="spotify-stub">Loading album…</div></div>
  const tracks = albumPlaybackTracks(page)
  const savedAlbum = spotifyMembership(page.savedAlbum, page.uri, membership)
  const refresh = () => setRevision((current) => current + 1)
  const trackIsSavedIndividually = (track: AlbumPageView['tracks'][number]) => spotifyMembership(track.savedIndividually, track.uri, membership)
  const rateAlbum = (stars: number) => void loadCurrentGeneration(entityRequestGeneration(ratingGenerations.current, `album:${page.uri}`), () => libraryGateway.setAlbumRating('music', page.artist, page.name, stars === page.albumRating ? null : stars), refresh, (error) => onError(String(error)))
  const rateTrack = (id: number, stars: number) => void loadCurrentGeneration(entityRequestGeneration(ratingGenerations.current, `track:${id}`), () => libraryGateway.clickTrackStar(id, stars), refresh, (error) => onError(String(error)))
  const remove = async () => {
    setBusy(true)
    try {
      if (await onRemove(page.uri)) refresh()
    } finally {
      setBusy(false)
    }
  }
  const add = async () => {
    if (await onAdd({ uri: page.uri, name: page.name, artist: page.artist })) refresh()
  }
  const toggleTrack = async (track: AlbumPageView['tracks'][number]) => {
    if (!beginPendingEntity(pendingTracks.current, track.uri)) return
    const savedIndividually = trackIsSavedIndividually(track)
    setTrackBusy(pendingEntities(pendingTracks.current))
    try {
      if (await (savedIndividually ? onRemoveTrack(track.uri) : onAddTrack(track.uri))) refresh()
    } finally {
      pendingTracks.current.delete(track.uri)
      setTrackBusy(pendingEntities(pendingTracks.current))
    }
  }
  const presentation: SpotifyAlbumPresentationData = {
    uri: page.uri,
    name: page.name,
    artist: page.artist,
    albumType: page.albumType,
    year: page.year,
    imageUrl: page.imageUrl,
    tracks: page.tracks.map((track) => ({ uri: track.uri, name: track.name, trackNo: track.trackNo, durationSecs: track.durationSecs })),
  }
  return <div className="spotify-page">
    <SpotifyPageBack label={backLabel} onBack={onBack} />
    <SpotifyAlbumPresentation
      album={presentation}
      titleAs="h1"
      artistContent={<button className="spotify-link artist-link" onClick={() => onArtist(page.artistId)}>{page.artist}</button>}
      headerMeta={<div className="spotify-page-meta"><RatingStars rating={page.albumRating} explicit onRate={page.contentComplete && !adding && !busy ? rateAlbum : undefined} /><span>{page.year && `${page.year} · `}{page.tracks.length} {page.tracks.length === 1 ? 'track' : 'tracks'} · {Math.floor(page.totalDurationSecs / 60)} min</span>{page.addedAt !== null && <time>Date Added: {new Date(page.addedAt * 1000).toLocaleDateString()}</time>}</div>}
      headerActions={<div className="spotify-page-actions">
        <button className="primary" onClick={() => onPlay(tracks[0].id, tracks)} disabled={!tracks.length}>▶ Play</button>
        {savedAlbum
          ? <button disabled={adding || busy} onClick={() => void remove()}>{busy ? 'Removing…' : adding ? 'Adding…' : '✓ In Library — Remove'}</button>
          : <button disabled={adding || busy} onClick={() => void add()}>{busy ? 'Removing…' : adding ? 'Adding…' : '+ Add to Library'}</button>}
      </div>}
      renderTrack={(_presentationTrack, index) => {
        const track = page.tracks[index]
        const subject: PlaylistSubject = { kind: 'tracks', label: `Track · ${track.name}`, uris: [track.uri] }
        const savedIndividually = trackIsSavedIndividually(track)
        const mutating = trackBusy.has(track.uri)
        return <div key={track.uri} ref={track.uri === entry.highlight ? highlighted : undefined} draggable aria-current={playingUri === track.uri ? 'true' : undefined} className={`spotify-track-row ${track.uri === entry.highlight ? 'highlighted' : ''}`} onDoubleClick={() => onPlay(tracks[index].id, tracks)} onDragStart={(event) => {
          event.dataTransfer.effectAllowed = 'copy'
          event.dataTransfer.setData(DRAG_TYPE, JSON.stringify(subject))
        }} onContextMenu={(event) => { event.preventDefault(); setMenu({ x: event.clientX, y: event.clientY, index }) }}>
        <span>{track.trackNo ?? index + 1}</span>
        <span>{track.name}</span>
        <RatingStars rating={track.rating?.stars ?? null} explicit={track.rating?.explicit} onRate={track.trackId === null ? undefined : (stars) => rateTrack(track.trackId!, stars)} />
        <time>{formatTime(track.durationSecs)}</time>
        <button className="spotify-track-action" draggable={false} title={`Play ${track.name}`} aria-label={`Play ${track.name}`} onClick={(event) => { event.stopPropagation(); onPlay(tracks[index].id, tracks) }} onDoubleClick={(event) => event.stopPropagation()}>▶ Play</button>
        <button className="spotify-track-action library" draggable={false} disabled={mutating} title={savedIndividually ? 'Remove from Library' : 'Add to Library'} aria-label={savedIndividually ? `Remove ${track.name} from Library` : `Add ${track.name} to Library`} onClick={(event) => { event.stopPropagation(); void toggleTrack(track) }} onDoubleClick={(event) => event.stopPropagation()}>{mutating ? savedIndividually ? 'Removing…' : 'Adding…' : savedIndividually ? '✓ Added' : '+ Add'}</button>
        {menu?.index === index && <ContextMenu x={menu.x} y={menu.y} onClose={() => setMenu(undefined)}><button onClick={() => { setMenu(undefined); onPlaylist(subject) }}>Add to Playlist…</button></ContextMenu>}
      </div>
      }}
      trackFooter={<p className="spotify-page-hint">Double-click a track to preview. Adding the album pulls every track into your local overlay.</p>}
    />
  </div>
}

function SpotifyArtistPage({ id, backLabel, adding, membership, onBack, onAlbum, onAdd, onRemove, onPlaylist, onError }: {
  id: string
  backLabel: string
  adding: ReadonlySet<string>
  membership: SpotifyMembershipOverrides
  onBack: () => void
  onAlbum: (uri: string) => void
  onAdd: (album: { uri: string; name: string; artist: string }) => Promise<boolean>
  onRemove: (uri: string) => Promise<boolean>
  onPlaylist: (subject: PlaylistSubject) => void
  onError: (error: string) => void
}) {
  const [loaded, setLoaded] = useState<
    | { id: string; status: 'loading' }
    | { id: string; status: 'error'; error: string }
    | { id: string; status: 'ready'; page: ArtistPageView }
  >({ id, status: 'loading' })
  const [albums, setAlbums] = useState<{ id: string; page: ArtistAlbumsPage }>({ id, page: { albums: [], nextOffset: 0, total: 0 } })
  const [loadingAlbums, setLoadingAlbums] = useState(true)
  const [albumsError, setAlbumsError] = useState<string>()
  const [toggling, setToggling] = useState(false)
  const artistGeneration = useRef(0)
  const albumsGeneration = useRef(0)
  const loadArtist = useCallback(() => {
    setLoaded({ id, status: 'loading' })
    void loadCurrentGeneration(artistGeneration,
      () => spotifyGateway.artistPage(id),
      (page) => setLoaded({ id, status: 'ready', page }),
      (error) => setLoaded({ id, status: 'error', error: String(error) }))
  }, [id])
  useEffect(() => {
    let active = true
    setAlbums({ id, page: { albums: [], nextOffset: 0, total: 0 } })
    setLoadingAlbums(true)
    setAlbumsError(undefined)
    loadArtist()
    void loadCurrentGeneration(albumsGeneration,
      () => spotifyGateway.artistAlbums(id, 0),
      (next) => {
        if (!active) return
        setAlbums({ id, page: next })
        setLoadingAlbums(false)
      },
      (error) => {
        if (!active) return
        setAlbumsError(String(error))
        setLoadingAlbums(false)
      })
    return () => { active = false; artistGeneration.current += 1; albumsGeneration.current += 1 }
  }, [id, loadArtist])
  const artist = loaded.id === id ? loaded : { id, status: 'loading' as const }
  const discography = albums.id === id ? albums.page : { albums: [], nextOffset: 0, total: 0 }
  if (artist.status === 'error') return <div className="spotify-page"><SpotifyPageBack label={backLabel} onBack={onBack} /><div className="spotify-stub"><p>Artist details are unavailable.</p><p>{artist.error}</p><button type="button" onClick={loadArtist}>Retry</button></div></div>
  if (artist.status !== 'ready' || artist.page.id !== id) return <div className="spotify-page"><SpotifyPageBack label={backLabel} onBack={onBack} /><div className="spotify-stub">Loading artist…</div></div>
  const page = artist.page
  const loadMore = async () => {
    if (discography.nextOffset === null || loadingAlbums) return
    const offset = discography.nextOffset
    setLoadingAlbums(true)
    setAlbumsError(undefined)
    await loadCurrentGeneration(albumsGeneration,
      () => spotifyGateway.artistAlbums(id, offset),
      (incoming) => {
        setAlbums((current) => current.id === id
          ? { id, page: { ...incoming, albums: mergeByUri(current.page.albums, incoming.albums) } }
          : current)
        setLoadingAlbums(false)
      },
      (error) => {
        setAlbumsError(String(error))
        setLoadingAlbums(false)
      })
  }
  const toggleFollow = async () => {
    const following = !page.following
    const next = { ...page, following }
    setLoaded({ id, status: 'ready', page: next })
    setToggling(true)
    try {
      await spotifyGateway.followArtist(page.id, following)
    } catch (error) {
      const restored = { ...page, following: !following }
      setLoaded({ id, status: 'ready', page: restored })
      onError(String(error))
    } finally {
      setToggling(false)
    }
  }
  return <div className="spotify-page">
    <SpotifyPageBack label={backLabel} onBack={onBack} />
    <header className="spotify-page-header artist-header">
      <div className="spotify-page-art artist-art"><SpotifyArtwork imageUrl={page.imageUrl} round /></div>
      <div className="spotify-page-copy">
        <div className="spotify-eyebrow">ARTIST</div>
        <h1>{page.name}</h1>
        <div className="spotify-page-meta">{page.descriptor}{discography.total ? ` · ${discography.total} albums and singles` : ''}</div>
        <div className="spotify-page-actions">
          <button disabled={toggling} onClick={() => void toggleFollow()}>{page.following ? '✓ Following' : '+ Follow'}</button>
        </div>
      </div>
    </header>
    <section className="spotify-page-section">
      <h2>Discography{discography.total ? ` · ${discography.albums.length} of ${discography.total}` : ''}</h2>
      {discography.albums.map((album) => <SpotifyAlbumRow key={album.uri} album={album} adding={adding.has(album.uri)} added={spotifyMembership(album.inLibrary, album.uri, membership)} onAdd={() => { void onAdd(album) }} onRemove={() => { void onRemove(album.uri) }} onOpen={() => onAlbum(album.uri)} onPlaylist={onPlaylist} showType />)}
      {loadingAlbums && <p>Loading albums…</p>}
      {albumsError && <div className="spotify-page-load-more"><span>{albumsError}</span><button onClick={() => void loadMore()}>Try again</button></div>}
      {!loadingAlbums && !albumsError && !discography.albums.length && discography.nextOffset === null && <p>No albums or singles found.</p>}
      {!loadingAlbums && !albumsError && discography.nextOffset !== null && <div className="spotify-page-load-more"><button onClick={() => void loadMore()}>Load more</button></div>}
    </section>
  </div>
}

export function SpotifySearch({ query, searching, results, navigation, playingUri, onAdd, onAddTrack, onRemoveTrack, onPlay, onPlaylist, onClose, onError }: {
  query: string
  searching: boolean
  results: SpotifyResults | null
  navigation?: SpotifyNavEntry
  playingUri: string | null
  onAdd: (album: { uri: string; name: string; artist: string }) => Promise<unknown>
  onAddTrack: (uri: string) => Promise<unknown>
  onRemoveTrack: (uri: string) => Promise<unknown>
  onPlay: (id: number, tracks: readonly PlaybackTrack[]) => void
  onPlaylist: (subject: PlaylistSubject) => void
  onClose: () => void
  onError: (error: string) => void
}) {
  const [searchState, setSearchState] = useState(() => createSpotifySearchState(query))
  const searchStateRef = useRef(searchState)
  const [adding, setAdding] = useState<Set<string>>(new Set())
  const pendingMembership = useRef(new Set<string>())
  const [playingAlbum, setPlayingAlbum] = useState<string>()
  const [membership, setMembership] = useState<Record<string, boolean>>({})
  const [nav, setNav] = useState<SpotifyNavEntry[]>(navigation ? [navigation] : [])
  const [menu, setMenu] = useState<{ x: number; y: number; track: SpotifyResults['tracks']['items'][number] }>()
  const pendingPages = useRef(new Map<string, Promise<SpotifyResults>>())
  searchStateRef.current = searchState
  useEffect(() => {
    setSearchState((current) => {
      const next = current.query === query ? current : resetSpotifySearchQuery(current, query)
      searchStateRef.current = next
      return next
    })
    setMembership({})
    setNav(navigation ? [navigation] : [])
  }, [query, navigation])
  useEffect(() => {
    if (!results) return
    setSearchState((current) => replaceSpotifySearchResults(current, results))
  }, [results])
  const requestGroup = (group: keyof SpotifyResults, retry = false) => {
    const current = searchStateRef.current
    const outcome = retry ? retrySpotifySearchGroup(current, group) : expandSpotifySearchGroup(current, group)
    searchStateRef.current = outcome.state
    setSearchState(outcome.state)
    if (!outcome.request) return
    const request = outcome.request
    const key = spotifySearchPendingPageKey(query, request.offset, request.generation)
    let page = pendingPages.current.get(key)
    if (!page) {
      page = spotifyGateway.search(query, request.offset)
        .then((next) => {
          pendingPages.current.delete(key)
          return next
        }, (error) => {
          pendingPages.current.delete(key)
          throw error
        })
      pendingPages.current.set(key, page)
    }
    page.then((next) => setSearchState((state) => receiveSpotifySearchPage(state, request.group, request.offset, next, request.generation)))
      .catch((error) => setSearchState((state) => failSpotifySearchGroup(state, request.group, String(error), request.generation)))
  }
  const mutateMembership = async (uri: string, saved: boolean, action: () => Promise<unknown>) => {
    if (!beginPendingEntity(pendingMembership.current, uri)) return false
    const hadOverride = uri in membership
    const previous = membership[uri]
    setMembership((current) => ({ ...current, [uri]: saved }))
    setAdding(pendingEntities(pendingMembership.current))
    try {
      await action()
      return true
    } catch {
      setMembership((current) => {
        const next = { ...current }
        if (hadOverride) next[uri] = previous
        else delete next[uri]
        return next
      })
      return false
    } finally {
      pendingMembership.current.delete(uri)
      setAdding(pendingEntities(pendingMembership.current))
    }
  }
  const add = (album: { uri: string; name: string; artist: string }) =>
    mutateMembership(album.uri, true, () => onAdd(album))
  const remove = (uri: string) => mutateMembership(uri, false, () =>
    spotifyGateway.removeAlbum(uri).catch((error) => { onError(String(error)); throw error }))
  const addTrack = (uri: string) => mutateMembership(uri, true, () => onAddTrack(uri))
  const removeTrack = (uri: string) => mutateMembership(uri, false, () => onRemoveTrack(uri))
  const playAlbum = async (album: SearchAlbum) => {
    setPlayingAlbum(album.uri)
    try {
      const page = await spotifyGateway.albumPage(album.uri)
      const tracks = albumPlaybackTracks(page)
      if (tracks.length) onPlay(tracks[0].id, tracks)
    } catch (error) {
      onError(String(error))
    } finally {
      setPlayingAlbum(undefined)
    }
  }
  const pushAlbum = (uri: string, highlight?: string) => setNav((current) => [...current, { kind: 'album', uri, highlight }])
  const top = nav[nav.length - 1]
  const below = nav[nav.length - 2]
  const backLabel = below?.kind ?? (navigation ? 'library' : 'results')
  const back = () => nav.length === 1 && navigation ? onClose() : setNav((current) => current.slice(0, -1))
  if (searching) return <div className="spotify-stub">Searching Spotify…</div>
  if (top?.kind === 'album') return <SpotifyAlbumPage key={top.uri} entry={top} backLabel={backLabel} adding={adding.has(top.uri)} membership={membership} playingUri={playingUri} onBack={back} onArtist={(id) => setNav((current) => [...current, { kind: 'artist', id }])} onAdd={add} onRemove={remove} onAddTrack={addTrack} onRemoveTrack={removeTrack} onPlay={onPlay} onPlaylist={onPlaylist} onError={onError} />
  if (top?.kind === 'artist') return <SpotifyArtistPage key={top.id} id={top.id} backLabel={backLabel} adding={adding} membership={membership} onBack={back} onAlbum={pushAlbum} onAdd={add} onRemove={remove} onPlaylist={onPlaylist} onError={onError} />
  const tab = searchState.tab
  const counts = {
    artists: searchState.groups.artists.total,
    albums: searchState.groups.albums.total,
    tracks: searchState.groups.tracks.total,
  }
  const tabs: { key: SpotifyTab; label: string; count: number }[] = [
    { key: 'all', label: 'All', count: counts.artists + counts.albums + counts.tracks },
    { key: 'artists', label: 'Artists', count: counts.artists },
    { key: 'albums', label: 'Albums', count: counts.albums },
    { key: 'tracks', label: 'Tracks', count: counts.tracks },
  ]
  return <div className="spotify-results-view">
    <div className="spotify-tabs" role="tablist" aria-label="Spotify result filters">
      {tabs.map((item) => <button key={item.key} role="tab" aria-selected={tab === item.key} className={tab === item.key ? 'active' : ''} onClick={() => setSearchState((current) => setSpotifySearchTab(current, item.key))}>{item.label} ({item.count})</button>)}
      <span>Spotify · &quot;{query}&quot;</span>
    </div>
    <div className="spotify-results">
      {(tab === 'all' || tab === 'artists') && <SpotifySearchSection group="artists" state={searchState} onMore={() => requestGroup('artists')} onRetry={() => requestGroup('artists', true)}>
        {searchState.groups.artists.items.slice(0, searchState.visible.artists).map((artist) => <SpotifyArtistRow key={artist.id} artist={artist} onOpen={() => setNav((current) => [...current, { kind: 'artist', id: artist.id }])} />)}
      </SpotifySearchSection>}
      {(tab === 'all' || tab === 'albums') && <SpotifySearchSection group="albums" state={searchState} onMore={() => requestGroup('albums')} onRetry={() => requestGroup('albums', true)}>
        {searchState.groups.albums.items.slice(0, searchState.visible.albums).map((album) => <SpotifyAlbumRow key={album.uri} album={album} adding={adding.has(album.uri)} added={spotifyMembership(album.inLibrary, album.uri, membership)} onAdd={() => { void add(album) }} onRemove={() => { void remove(album.uri) }} onOpen={() => pushAlbum(album.uri)} onPlaylist={onPlaylist} searchActions onPlay={() => { void playAlbum(album) }} playing={playingAlbum === album.uri || playingUri === album.uri} />)}
      </SpotifySearchSection>}
      {(tab === 'all' || tab === 'tracks') && <SpotifySearchSection group="tracks" state={searchState} onMore={() => requestGroup('tracks')} onRetry={() => requestGroup('tracks', true)}>
        {searchState.groups.tracks.items.slice(0, searchState.visible.tracks).map((track, index) => {
          const open = () => { if (track.albumUri) pushAlbum(track.albumUri, track.uri) }
          const playback = searchTrackPlayback(track, index)
          return <div className="spotify-row spotify-search-row" key={track.uri} onDoubleClick={open} onContextMenu={(event) => { event.preventDefault(); setMenu({ x: event.clientX, y: event.clientY, track }) }}>
            <SpotifyArtwork imageUrl={track.imageUrl} />
            <span className="spotify-copy"><strong>{track.name}</strong><small>{track.artist} · {track.alb}</small></span>
            <SpotifyResultActions name={track.name} meta={formatTime(track.durationSecs)} onPlay={() => onPlay(playback.id, [playback])} playing={playingUri === track.uri} adding={adding.has(track.uri)} added={spotifyMembership(track.inLibrary, track.uri, membership)} onAdd={() => { void addTrack(track.uri) }} onRemove={() => { void removeTrack(track.uri) }} onOpen={track.albumUri ? open : undefined} />
          </div>
        })}
      </SpotifySearchSection>}
    </div>
    {menu && <ContextMenu x={menu.x} y={menu.y} onClose={() => setMenu(undefined)}>
      <button onClick={() => {
        const track = menu.track
        setMenu(undefined)
        onPlaylist({ kind: 'tracks', label: `Track · ${track.name}`, uris: [track.uri] })
      }}>Add to Playlist…</button>
      <button disabled={!menu.track.albumUri} onClick={() => { const track = menu.track; setMenu(undefined); if (track.albumUri) pushAlbum(track.albumUri, track.uri) }}>Go to Album</button>
    </ContextMenu>}
  </div>
}
