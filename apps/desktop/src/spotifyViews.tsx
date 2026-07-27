import { invoke } from '@tauri-apps/api/core'
import { useEffect, useRef, useState } from 'react'
import type { AlbumPageView, ArtistAlbumsPage, ArtistPageView, PlaybackTrack, PlaylistSubject, SearchAlbum, SpotifyNavEntry, SpotifyResults } from './types.ts'
import { DRAG_TYPE, formatTime, mergeByUri, SYNTHETIC_BASE } from './ui.ts'
import { ContextMenu, RatingStars } from './viewShared.tsx'

type SpotifyTab = 'all' | keyof SpotifyResults

function SpotifyArtwork({ imageUrl, round = false }: { imageUrl: string | null; round?: boolean }) {
  return <span className={`spotify-artwork ${round ? 'round' : ''}`}>{imageUrl ? <img src={imageUrl} alt="" /> : <span aria-hidden="true">♪</span>}</span>
}

function SpotifyAlbumRow({ album, adding, added, onAdd, onRemove, onOpen, onPlaylist, openOnClick = false, showType = false }: {
  album: SearchAlbum
  adding: boolean
  added: boolean
  onAdd: () => void
  onRemove: () => void
  onOpen: () => void
  onPlaylist: (subject: PlaylistSubject) => void
  openOnClick?: boolean
  showType?: boolean
}) {
  const [menu, setMenu] = useState<{ x: number; y: number }>()
  const subject: PlaylistSubject = { kind: 'album', label: `Album · ${album.name}`, albumUri: album.uri }
  return <div className="spotify-row" draggable onClick={openOnClick ? onOpen : undefined} onDoubleClick={openOnClick ? undefined : onOpen} onDragStart={(event) => {
    event.dataTransfer.effectAllowed = 'copy'
    event.dataTransfer.setData(DRAG_TYPE, JSON.stringify(subject))
  }} onContextMenu={(event) => { event.preventDefault(); setMenu({ x: event.clientX, y: event.clientY }) }}>
    <SpotifyArtwork imageUrl={album.imageUrl} />
    <span className="spotify-copy"><strong>{album.name}</strong><small>{showType ? [album.year, album.albumType].filter(Boolean).join(' · ') : <>{album.artist}{album.year && ` · ${album.year}`}</>}</small></span>
    <button className="spotify-add" disabled={adding} title={added ? 'Remove from Library' : 'Add to Library'} aria-label={added ? `Remove ${album.name} from Library` : `Add ${album.name} to Library`} onClick={(event) => { event.stopPropagation(); if (added) onRemove(); else onAdd() }} onDoubleClick={(event) => event.stopPropagation()}>{adding ? added ? 'Removing…' : 'Adding…' : added ? '✓ In Library' : '+ Add'}</button>
    {menu && <ContextMenu x={menu.x} y={menu.y} onClose={() => setMenu(undefined)}><button onClick={() => { setMenu(undefined); onPlaylist(subject) }}>Add to Playlist…</button></ContextMenu>}
  </div>
}

function SpotifyPageBack({ label, onBack }: { label: string; onBack: () => void }) {
  return <button className="spotify-page-back" onClick={onBack}>‹ Back to {label}</button>
}

function SpotifyAlbumPage({ entry, backLabel, adding, onBack, onArtist, onAdd, onRemove, onPlay, onPlaylist, onError }: {
  entry: Extract<SpotifyNavEntry, { kind: 'album' }>
  backLabel: string
  adding: boolean
  onBack: () => void
  onArtist: (id: string) => void
  onAdd: (album: SearchAlbum) => Promise<boolean>
  onRemove: (album: SearchAlbum) => Promise<boolean>
  onPlay: (id: number, tracks: readonly PlaybackTrack[]) => void
  onPlaylist: (subject: PlaylistSubject) => void
  onError: (error: string) => void
}) {
  const [page, setPage] = useState<AlbumPageView>()
  const [revision, setRevision] = useState(0)
  const [busy, setBusy] = useState(false)
  const [trackBusy, setTrackBusy] = useState<string>()
  const [trackMembership, setTrackMembership] = useState<Record<string, boolean>>({})
  const [menu, setMenu] = useState<{ x: number; y: number; index: number }>()
  const highlighted = useRef<HTMLDivElement>(null)
  useEffect(() => {
    setTrackMembership({})
    setTrackBusy(undefined)
  }, [entry.uri])
  useEffect(() => {
    let active = true
    setPage(undefined)
    invoke<AlbumPageView>('spotify_album_page', { uri: entry.uri })
      .then((view) => active && setPage(view))
      .catch((error) => active && onError(String(error)))
    return () => { active = false }
  }, [entry.uri, revision])
  useEffect(() => {
    if (page && entry.highlight) highlighted.current?.scrollIntoView({ block: 'center' })
  }, [entry.highlight, page])
  if (!page) return <div className="spotify-page"><SpotifyPageBack label={backLabel} onBack={onBack} /><div className="spotify-stub">Loading album…</div></div>
  const tracks: PlaybackTrack[] = page.tracks.map((track, index) => ({
    id: track.trackId ?? SYNTHETIC_BASE + index,
    uri: track.uri,
    name: track.name,
    art: page.artist,
    alb: page.name,
    durationSecs: track.durationSecs,
  }))
  const refresh = () => setRevision((current) => current + 1)
  const trackIsInLibrary = (track: AlbumPageView['tracks'][number]) => trackMembership[track.uri] ?? track.trackId !== null
  const albumInLibrary = page.tracks.length > 0 && page.tracks.every(trackIsInLibrary)
  const setAllTrackMembership = (inLibrary: boolean) => setTrackMembership(Object.fromEntries(page.tracks.map((track) => [track.uri, inLibrary])))
  const rateAlbum = (stars: number) => invoke('set_album_rating', {
    source: 'music', art: page.artist, alb: page.name, stars: stars === page.albumRating ? null : stars,
  }).then(refresh).catch((error) => onError(String(error)))
  const rateTrack = (id: number, stars: number) => invoke('click_track_star', { id, stars }).then(refresh).catch((error) => onError(String(error)))
  const remove = async () => {
    const previous = trackMembership
    setAllTrackMembership(false)
    setBusy(true)
    try {
      if (await onRemove({ uri: page.uri, name: page.name, artist: page.artist, year: page.year, imageUrl: page.imageUrl, albumType: page.albumType, trackCount: page.tracks.length, inLibrary: albumInLibrary })) refresh()
      else setTrackMembership(previous)
    } finally {
      setBusy(false)
    }
  }
  const add = async () => {
    const previous = trackMembership
    setAllTrackMembership(true)
    if (await onAdd({ uri: page.uri, name: page.name, artist: page.artist, year: page.year, imageUrl: page.imageUrl, albumType: page.albumType, trackCount: page.tracks.length, inLibrary: albumInLibrary })) refresh()
    else setTrackMembership(previous)
  }
  const toggleTrack = async (track: AlbumPageView['tracks'][number]) => {
    const inLibrary = trackIsInLibrary(track)
    setTrackBusy(track.uri)
    setTrackMembership((current) => ({ ...current, [track.uri]: !inLibrary }))
    try {
      await invoke(inLibrary ? 'remove_spotify_track' : 'add_spotify_track', { uri: track.uri })
      refresh()
    } catch (error) {
      setTrackMembership((current) => ({ ...current, [track.uri]: inLibrary }))
      onError(String(error))
    } finally {
      setTrackBusy(undefined)
    }
  }
  return <div className="spotify-page">
    <SpotifyPageBack label={backLabel} onBack={onBack} />
    <header className="spotify-page-header album-header">
      <div className="spotify-page-art album-art"><SpotifyArtwork imageUrl={page.imageUrl} /></div>
      <div className="spotify-page-copy">
        <div className="spotify-eyebrow">ALBUM{page.albumType.toLowerCase() !== 'album' && ` · ${page.albumType.toUpperCase()}`}</div>
        <h1>{page.name}</h1>
        <button className="spotify-link artist-link" onClick={() => onArtist(page.artistId)}>{page.artist}</button>
        <div className="spotify-page-meta"><RatingStars rating={page.albumRating} explicit onRate={albumInLibrary && !adding && !busy ? rateAlbum : undefined} /><span>{page.year && `${page.year} · `}{page.tracks.length} {page.tracks.length === 1 ? 'track' : 'tracks'} · {Math.floor(page.totalDurationSecs / 60)} min</span></div>
        <div className="spotify-page-actions">
          <button className="primary" onClick={() => onPlay(tracks[0].id, tracks)} disabled={!tracks.length}>▶ Play</button>
          {albumInLibrary
            ? <button disabled={adding || busy} onClick={() => void remove()}>{busy ? 'Removing…' : adding ? 'Adding…' : '✓ In Library — Remove'}</button>
            : <button disabled={adding || busy} onClick={() => void add()}>{busy ? 'Removing…' : adding ? 'Adding…' : '+ Add to Library'}</button>}
        </div>
      </div>
    </header>
    <section className="spotify-page-section album-tracks">
      {page.tracks.map((track, index) => {
        const subject: PlaylistSubject = { kind: 'tracks', label: `Track · ${track.name}`, uris: [track.uri] }
        const inLibrary = trackIsInLibrary(track)
        const mutating = trackBusy === track.uri
        return <div key={track.uri} ref={track.uri === entry.highlight ? highlighted : undefined} draggable className={`spotify-track-row ${track.uri === entry.highlight ? 'highlighted' : ''}`} onDoubleClick={() => onPlay(tracks[index].id, tracks)} onDragStart={(event) => {
          event.dataTransfer.effectAllowed = 'copy'
          event.dataTransfer.setData(DRAG_TYPE, JSON.stringify(subject))
        }} onContextMenu={(event) => { event.preventDefault(); setMenu({ x: event.clientX, y: event.clientY, index }) }}>
        <span>{track.trackNo ?? index + 1}</span>
        <span>{track.name}</span>
        <RatingStars rating={track.rating?.stars ?? null} explicit={track.rating?.explicit} onRate={track.trackId === null ? undefined : (stars) => rateTrack(track.trackId!, stars)} />
        <time>{formatTime(track.durationSecs)}</time>
        <button className="spotify-track-action" draggable={false} title={`Play ${track.name}`} aria-label={`Play ${track.name}`} onClick={(event) => { event.stopPropagation(); onPlay(tracks[index].id, tracks) }} onDoubleClick={(event) => event.stopPropagation()}>▶ Play</button>
        <button className="spotify-track-action library" draggable={false} disabled={mutating} title={inLibrary ? 'Remove from Library' : 'Add to Library'} aria-label={inLibrary ? `Remove ${track.name} from Library` : `Add ${track.name} to Library`} onClick={(event) => { event.stopPropagation(); void toggleTrack(track) }} onDoubleClick={(event) => event.stopPropagation()}>{mutating ? inLibrary ? 'Adding…' : 'Removing…' : inLibrary ? '✓ Added' : '+ Add'}</button>
        {menu?.index === index && <ContextMenu x={menu.x} y={menu.y} onClose={() => setMenu(undefined)}><button onClick={() => { setMenu(undefined); onPlaylist(subject) }}>Add to Playlist…</button></ContextMenu>}
      </div>})}
      <p className="spotify-page-hint">Double-click a track to preview. Adding the album pulls every track into your local overlay.</p>
    </section>
  </div>
}

const artistPageCache = new Map<string, ArtistPageView>()
const artistPageRequests = new Map<string, Promise<ArtistPageView>>()
const artistAlbumsCache = new Map<string, ArtistAlbumsPage>()
const artistAlbumRequests = new Map<string, Promise<ArtistAlbumsPage>>()

function getArtistPage(id: string) {
  const cached = artistPageCache.get(id)
  if (cached) return Promise.resolve(cached)
  const pending = artistPageRequests.get(id)
  if (pending) return pending
  const request = invoke<ArtistPageView>('spotify_artist_page', { artistId: id })
    .then((page) => { artistPageCache.set(id, page); return page })
    .finally(() => artistPageRequests.delete(id))
  artistPageRequests.set(id, request)
  return request
}

function getArtistAlbumsPage(id: string, offset: number) {
  const key = `${id}:${offset}`
  const pending = artistAlbumRequests.get(key)
  if (pending) return pending
  const request = invoke<ArtistAlbumsPage>('spotify_artist_albums', { artistId: id, offset })
    .finally(() => artistAlbumRequests.delete(key))
  artistAlbumRequests.set(key, request)
  return request
}

function SpotifyArtistPage({ id, backLabel, adding, added, removed, onBack, onAlbum, onAdd, onRemove, onPlaylist, onError }: {
  id: string
  backLabel: string
  adding: string | undefined
  added: ReadonlySet<string>
  removed: ReadonlySet<string>
  onBack: () => void
  onAlbum: (uri: string) => void
  onAdd: (album: SearchAlbum) => Promise<boolean>
  onRemove: (album: SearchAlbum) => Promise<boolean>
  onPlaylist: (subject: PlaylistSubject) => void
  onError: (error: string) => void
}) {
  const [page, setPage] = useState<ArtistPageView | undefined>(() => artistPageCache.get(id))
  const [discography, setDiscography] = useState<ArtistAlbumsPage>(() => artistAlbumsCache.get(id) ?? { albums: [], nextOffset: 0, total: 0 })
  const [loadingAlbums, setLoadingAlbums] = useState(!artistAlbumsCache.has(id))
  const [albumsError, setAlbumsError] = useState<string>()
  const [toggling, setToggling] = useState(false)
  useEffect(() => {
    let active = true
    setPage(artistPageCache.get(id))
    const cachedAlbums = artistAlbumsCache.get(id)
    setDiscography(cachedAlbums ?? { albums: [], nextOffset: 0, total: 0 })
    setLoadingAlbums(!cachedAlbums)
    setAlbumsError(undefined)
    getArtistPage(id)
      .then((view) => active && setPage(view))
      .catch((error) => active && onError(String(error)))
    if (!cachedAlbums) getArtistAlbumsPage(id, 0)
      .then((next) => {
        if (!active) return
        artistAlbumsCache.set(id, next)
        setDiscography(next)
      })
      .catch((error) => active && setAlbumsError(String(error)))
      .finally(() => active && setLoadingAlbums(false))
    return () => { active = false }
  }, [id])
  if (!page || page.id !== id) return <div className="spotify-page"><SpotifyPageBack label={backLabel} onBack={onBack} /><div className="spotify-stub">Loading artist…</div></div>
  const loadMore = async () => {
    if (discography.nextOffset === null || loadingAlbums) return
    setLoadingAlbums(true)
    setAlbumsError(undefined)
    try {
      const incoming = await getArtistAlbumsPage(id, discography.nextOffset)
      setDiscography((current) => {
        const next = { ...incoming, albums: mergeByUri(current.albums, incoming.albums) }
        artistAlbumsCache.set(id, next)
        return next
      })
    } catch (error) {
      setAlbumsError(String(error))
    } finally {
      setLoadingAlbums(false)
    }
  }
  const toggleFollow = async () => {
    const following = !page.following
    const next = { ...page, following }
    artistPageCache.set(id, next)
    setPage(next)
    setToggling(true)
    try {
      await invoke('spotify_follow_artist', { artistId: page.id, follow: following })
    } catch (error) {
      const restored = { ...page, following: !following }
      artistPageCache.set(id, restored)
      setPage(restored)
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
      {discography.albums.map((album) => <SpotifyAlbumRow key={album.uri} album={album} adding={adding === album.uri} added={(album.inLibrary || added.has(album.uri)) && !removed.has(album.uri)} onAdd={() => { void onAdd(album) }} onRemove={() => { void onRemove(album) }} onOpen={() => onAlbum(album.uri)} onPlaylist={onPlaylist} openOnClick showType />)}
      {loadingAlbums && <p>Loading albums…</p>}
      {albumsError && <div className="spotify-page-load-more"><span>{albumsError}</span><button onClick={() => void loadMore()}>Try again</button></div>}
      {!loadingAlbums && !albumsError && !discography.albums.length && discography.nextOffset === null && <p>No albums or singles found.</p>}
      {!loadingAlbums && !albumsError && discography.nextOffset !== null && <div className="spotify-page-load-more"><button onClick={() => void loadMore()}>Load more</button></div>}
    </section>
  </div>
}

export function SpotifySearch({ query, searching, results, navigation, onAdd, onPlay, onPlaylist, onClose, onError }: {
  query: string
  searching: boolean
  results: SpotifyResults | null
  navigation?: SpotifyNavEntry
  onAdd: (album: SpotifyResults['albums'][number]) => Promise<unknown>
  onPlay: (id: number, tracks: readonly PlaybackTrack[]) => void
  onPlaylist: (subject: PlaylistSubject) => void
  onClose: () => void
  onError: (error: string) => void
}) {
  const [tab, setTab] = useState<SpotifyTab>('all')
  const [adding, setAdding] = useState<string>()
  const [added, setAdded] = useState<ReadonlySet<string>>(new Set())
  const [removed, setRemoved] = useState<ReadonlySet<string>>(new Set())
  const [nav, setNav] = useState<SpotifyNavEntry[]>(navigation ? [navigation] : [])
  const [menu, setMenu] = useState<{ x: number; y: number; track: SpotifyResults['tracks'][number] }>()
  useEffect(() => {
    setTab('all')
    setAdded(new Set())
    setRemoved(new Set())
    setNav(navigation ? [navigation] : [])
  }, [query, navigation])
  const add = async (album: SpotifyResults['albums'][number]) => {
    setAdding(album.uri)
    try {
      await onAdd(album)
      setAdded((previous) => new Set(previous).add(album.uri))
      setRemoved((previous) => { const next = new Set(previous); next.delete(album.uri); return next })
      return true
    } catch {
      return false
    } finally {
      setAdding(undefined)
    }
  }
  const remove = async (album: SpotifyResults['albums'][number]) => {
    setAdding(album.uri)
    try {
      await invoke('remove_spotify_album', { uri: album.uri })
      setRemoved((previous) => new Set(previous).add(album.uri))
      setAdded((previous) => { const next = new Set(previous); next.delete(album.uri); return next })
      return true
    } catch (error) {
      onError(String(error))
      return false
    } finally {
      setAdding(undefined)
    }
  }
  const pushAlbum = (uri: string, highlight?: string) => setNav((current) => [...current, { kind: 'album', uri, highlight }])
  const top = nav[nav.length - 1]
  const below = nav[nav.length - 2]
  const backLabel = below?.kind ?? (navigation ? 'library' : 'results')
  const back = () => nav.length === 1 && navigation ? onClose() : setNav((current) => current.slice(0, -1))
  if (searching) return <div className="spotify-stub">Searching Spotify…</div>
  if (top?.kind === 'album') return <SpotifyAlbumPage entry={top} backLabel={backLabel} adding={adding === top.uri} onBack={back} onArtist={(id) => setNav((current) => [...current, { kind: 'artist', id }])} onAdd={add} onRemove={remove} onPlay={onPlay} onPlaylist={onPlaylist} onError={onError} />
  if (top?.kind === 'artist') return <SpotifyArtistPage id={top.id} backLabel={backLabel} adding={adding} added={added} removed={removed} onBack={back} onAlbum={pushAlbum} onAdd={add} onRemove={remove} onPlaylist={onPlaylist} onError={onError} />
  const counts = {
    artists: results?.artists.length ?? 0,
    albums: results?.albums.length ?? 0,
    tracks: results?.tracks.length ?? 0,
  }
  const tabs: { key: SpotifyTab; label: string; count: number }[] = [
    { key: 'all', label: 'All', count: counts.artists + counts.albums + counts.tracks },
    { key: 'artists', label: 'Artists', count: counts.artists },
    { key: 'albums', label: 'Albums', count: counts.albums },
    { key: 'tracks', label: 'Tracks', count: counts.tracks },
  ]
  return <div className="spotify-results-view">
    <div className="spotify-tabs" role="tablist" aria-label="Spotify result filters">
      {tabs.map((item) => <button key={item.key} role="tab" aria-selected={tab === item.key} className={tab === item.key ? 'active' : ''} onClick={() => setTab(item.key)}>{item.label} ({item.count})</button>)}
      <span>Spotify · &quot;{query}&quot;</span>
    </div>
    <div className="spotify-results">
      {(tab === 'all' || tab === 'artists') && <section>
        {tab === 'all' && <h2>Artists</h2>}
        {results?.artists.map((artist) => <div className="spotify-row" key={artist.id}>
          <SpotifyArtwork imageUrl={artist.imageUrl} round />
          <span className="spotify-copy"><strong>{artist.name}</strong><small>{artist.descriptor}</small></span>
          <button className="spotify-link" onClick={() => setNav((current) => [...current, { kind: 'artist', id: artist.id }])}>View albums ›</button>
        </div>)}
        {!results?.artists.length && <p>No artists found.</p>}
      </section>}
      {(tab === 'all' || tab === 'albums') && <section>
        {tab === 'all' && <h2>Albums</h2>}
        {results?.albums.map((album) => <SpotifyAlbumRow key={album.uri} album={album} adding={adding === album.uri} added={(album.inLibrary || added.has(album.uri)) && !removed.has(album.uri)} onAdd={() => { void add(album) }} onRemove={() => { void remove(album) }} onOpen={() => pushAlbum(album.uri)} onPlaylist={onPlaylist} />)}
        {!results?.albums.length && <p>No albums found.</p>}
      </section>}
      {(tab === 'all' || tab === 'tracks') && <section>
        {tab === 'all' && <h2>Tracks</h2>}
        {results?.tracks.map((track) => <div className="spotify-row" key={track.uri} onContextMenu={(event) => { event.preventDefault(); setMenu({ x: event.clientX, y: event.clientY, track }) }}>
          <SpotifyArtwork imageUrl={track.imageUrl} />
          <span className="spotify-copy"><strong>{track.name}</strong><small>{track.artist} · {track.alb}</small></span>
          <time>{Math.floor(track.durationSecs / 60)}:{String(Math.floor(track.durationSecs % 60)).padStart(2, '0')}</time>
        </div>)}
        {!results?.tracks.length && <p>No tracks found.</p>}
      </section>}
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
