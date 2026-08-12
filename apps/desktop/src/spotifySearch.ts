import type { SearchAlbum, SearchArtist, SearchTrack, SpotifyResultGroupKey, SpotifyResults } from './types.ts'

export type SpotifySearchTab = 'all' | SpotifyResultGroupKey
export type SpotifySearchGroups = { [K in SpotifyResultGroupKey]: SpotifyResults[K] }
export type SpotifySearchState = {
  query: string
  tab: SpotifySearchTab
  pages: ReadonlyMap<number, SpotifyResults>
  groups: SpotifySearchGroups
  visible: Record<SpotifyResultGroupKey, number>
  loading: ReadonlySet<SpotifyResultGroupKey>
  errors: Partial<Record<SpotifyResultGroupKey, string>>
  generation: number
}
export type SpotifySearchRequest = { group: SpotifyResultGroupKey; offset: number; generation: number }

export const spotifySearchPendingPageKey = (query: string, offset: number, generation: number) => `${query}:${offset}:${generation}`

const GROUPS: SpotifyResultGroupKey[] = ['artists', 'albums', 'tracks']
const groupName = (group: SpotifyResultGroupKey) => group
const title = (group: SpotifyResultGroupKey) => group[0].toUpperCase() + group.slice(1)

export const spotifySearchVisibleCounts = (tab: SpotifySearchTab): Record<SpotifyResultGroupKey, number> => ({
  artists: tab === 'artists' ? 10 : 5,
  albums: tab === 'albums' ? 10 : 5,
  tracks: tab === 'tracks' ? 10 : 5,
})

const itemKey = (group: SpotifyResultGroupKey, item: SearchArtist | SearchAlbum | SearchTrack) =>
  group === 'artists' ? (item as SearchArtist).id : (item as SearchAlbum | SearchTrack).uri

function mergeGroupPages<K extends SpotifyResultGroupKey>(pages: ReadonlyMap<number, SpotifyResults>, group: K): SpotifyResults[K] {
  const items: (SearchArtist | SearchAlbum | SearchTrack)[] = []
  const seen = new Set<string>()
  let total = 0
  let nextOffset: number | null = null
  for (const [, page] of [...pages.entries()].sort(([left], [right]) => left - right)) {
    const current = page[group]
    total = current.total
    nextOffset = current.nextOffset
    for (const item of current.items) {
      if (seen.has(itemKey(group, item))) continue
      seen.add(itemKey(group, item))
      items.push(item)
    }
  }
  return { items, total, nextOffset } as SpotifyResults[K]
}

const groupsFromPages = (pages: ReadonlyMap<number, SpotifyResults>): SpotifySearchGroups => ({
  artists: mergeGroupPages(pages, 'artists'),
  albums: mergeGroupPages(pages, 'albums'),
  tracks: mergeGroupPages(pages, 'tracks'),
})

export const createSpotifySearchState = (query: string): SpotifySearchState => ({
  query,
  tab: 'all',
  pages: new Map(),
  groups: groupsFromPages(new Map()),
  visible: spotifySearchVisibleCounts('all'),
  loading: new Set(),
  errors: {},
  generation: 0,
})

export const resetSpotifySearchQuery = (state: SpotifySearchState, query: string) => ({
  ...createSpotifySearchState(query),
  generation: state.generation + 1,
})

export const replaceSpotifySearchResults = (state: SpotifySearchState, results: SpotifyResults): SpotifySearchState => {
  const pages = new Map([[0, results]])
  return { ...state, pages, groups: groupsFromPages(pages), loading: new Set(), errors: {} }
}

export const setSpotifySearchTab = (state: SpotifySearchState, tab: SpotifySearchTab): SpotifySearchState =>
  state.tab === tab
    ? state
    : { ...state, tab, visible: spotifySearchVisibleCounts(tab), loading: new Set(), errors: {}, generation: state.generation + 1 }

export const expandSpotifySearchGroup = (state: SpotifySearchState, group: SpotifyResultGroupKey): { state: SpotifySearchState; request?: SpotifySearchRequest } => {
  if (state.loading.has(group)) return { state }
  const visible = state.visible[group] + 10
  const nextState = { ...state, visible: { ...state.visible, [group]: visible }, errors: { ...state.errors } }
  delete nextState.errors[group]
  const cached = nextState.groups[group]
  if (cached.items.length >= visible || cached.nextOffset === null) return { state: nextState }
  const loading = new Set(nextState.loading)
  loading.add(group)
  return {
    state: { ...nextState, loading },
    request: { group, offset: cached.nextOffset, generation: state.generation },
  }
}

export const retrySpotifySearchGroup = (state: SpotifySearchState, group: SpotifyResultGroupKey): { state: SpotifySearchState; request?: SpotifySearchRequest } => {
  if (state.loading.has(group) || state.groups[group].nextOffset === null) return { state }
  const loading = new Set(state.loading)
  loading.add(group)
  const errors = { ...state.errors }
  delete errors[group]
  return {
    state: { ...state, visible: { ...state.visible, [group]: state.visible[group] + 10 }, loading, errors },
    request: { group, offset: state.groups[group].nextOffset, generation: state.generation },
  }
}

export const receiveSpotifySearchPage = (state: SpotifySearchState, group: SpotifyResultGroupKey, offset: number, page: SpotifyResults, generation: number): SpotifySearchState => {
  if (generation !== state.generation) return state
  const pages = new Map(state.pages)
  pages.set(offset, page)
  const loading = new Set(state.loading)
  loading.delete(group)
  const errors = { ...state.errors }
  delete errors[group]
  return { ...state, pages, groups: groupsFromPages(pages), loading, errors }
}

export const failSpotifySearchGroup = (state: SpotifySearchState, group: SpotifyResultGroupKey, error: string, generation: number): SpotifySearchState => {
  if (generation !== state.generation) return state
  const loading = new Set(state.loading)
  loading.delete(group)
  return {
    ...state,
    visible: { ...state.visible, [group]: Math.max(0, state.visible[group] - 10) },
    loading,
    errors: { ...state.errors, [group]: error },
  }
}

export const moreSpotifySearchLabel = (state: SpotifySearchState, group: SpotifyResultGroupKey) => {
  const remaining = state.groups[group].total - Math.min(state.visible[group], state.groups[group].total)
  return remaining > 0 && state.groups[group].nextOffset !== null
    ? `View ${Math.min(10, remaining)} more ${groupName(group)}`
    : undefined
}

export const spotifySearchGroupHeader = (state: SpotifySearchState, group: SpotifyResultGroupKey) => {
  const current = state.groups[group]
  const shown = Math.min(state.visible[group], current.total)
  return current.nextOffset === null || shown >= current.total
    ? title(group)
    : `${title(group)} · ${shown} of ${current.total}`
}

export const spotifySearchGroupKeys = GROUPS
