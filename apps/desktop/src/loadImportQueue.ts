import { lastfmGateway } from './lastfmGateway.ts'
import type { ImportQueueItem } from './lastfmImportState.ts'

const importQueuePageLimit = 1000

// ponytail: the current refresh still loads all rows for global sort/selection; use revisioned snapshots if steady refresh cost matters.
export async function loadImportQueue(isCurrent: () => boolean): Promise<ImportQueueItem[]> {
  for (let attempt = 0; attempt < 2; attempt += 1) {
    const items: ImportQueueItem[] = []
    let cursor = 0
    let total: number | undefined
    while (true) {
      if (!isCurrent()) return []
      const page = await lastfmGateway.queue(cursor, importQueuePageLimit)
      if (!isCurrent()) return []
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

