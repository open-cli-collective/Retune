import { tauriInvoker, type Invoker } from './ipc.ts'
import type { BrowseView, MetadataValues, Selection, Source, TrackInfo } from './types.ts'

type TrackEdit = Partial<Pick<TrackInfo, 'name' | 'art' | 'alb' | 'cat'>> & {
  ratingChange?: { stars: number | null }
}

export const libraryEvents = {
  changed: 'library-changed',
  localImportStarted: 'local-import-started',
  localImportFailed: 'local-import-failed',
  localDragChanged: 'local-drag-changed',
} as const

export function createLibraryGateway(invoke: Invoker) {
  return {
    browse(source: Source, sel: Selection, query?: string | null) {
      return invoke<BrowseView>('browse', {
        source,
        sel: { cat: sel.cat ?? [], art: sel.art ?? [], alb: sel.alb ?? [] },
        ...(query == null ? {} : { query }),
      })
    },
    metadataValues: () => invoke<MetadataValues>('metadata_values'),
    genreValues: () => invoke<string[]>('genre_values'),
    getTrack: (id: number) => invoke<TrackInfo>('get_track', { id }),
    editTrack: (id: number, edit: TrackEdit) => invoke<void>('edit_track', { id, edit }),
    editTracks: (ids: number[], edit: TrackEdit) => invoke<void>('set_track_infos', { ids, edit }),
    clickTrackStar: (id: number, stars: number) => invoke<void>('click_track_star', { id, stars }),
    setTrackEnabled: (id: number, enabled: boolean) => invoke<void>('set_track_enabled', { id, enabled }),
    setAlbumRating: (source: Source, art: string, alb: string, stars: number | null) => invoke<void>('set_album_rating', { source, art, alb, stars }),
  }
}

export const libraryGateway = createLibraryGateway(tauriInvoker)
