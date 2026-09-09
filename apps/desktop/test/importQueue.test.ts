import { afterEach, expect, test, vi } from 'vitest'
import { lastfmGateway } from '../src/lastfmGateway.ts'
import { loadImportQueue } from '../src/loadImportQueue.ts'
import { applyCurrentImportRefresh } from '../src/lastfmImportState.ts'
import type { ImportQueueItem } from '../src/lastfmImportState.ts'

afterEach(() => vi.restoreAllMocks())

test('superseded queue loads stop after their pending page; newest load retains the complete queue', async () => {
  const items = Array.from({ length: 2100 }, (_, index) => ({ page: index + 1 }) as ImportQueueItem)
  let finishOld!: (value: Awaited<ReturnType<typeof lastfmGateway.queue>>) => void
  const page = (cursor: number) => ({ cursor, items: items.slice(cursor, cursor + 1000), total: items.length, nextCursor: cursor + 1000 < items.length ? cursor + 1000 : null })
  const queue = vi.spyOn(lastfmGateway, 'queue').mockImplementationOnce(() => new Promise(resolve => { finishOld = resolve }))
    .mockImplementation(async cursor => page(cursor))
  const generation = { current: 1 }
  const apply = vi.fn()
  const old = applyCurrentImportRefresh(1, generation, loadImportQueue(() => generation.current === 1), apply)
  generation.current = 2
  const newest = loadImportQueue(() => generation.current === 2)
  finishOld(page(0))
  expect((await old).applied).toBe(false)
  expect(apply).not.toHaveBeenCalled()
  expect(await newest).toEqual(items)
  expect(queue.mock.calls.map(([cursor]) => cursor)).toEqual([0, 0, 1000, 2000])
  queue.mockClear()
  expect(await loadImportQueue(() => false)).toEqual([])
  expect(queue).not.toHaveBeenCalled()
})

test('current queue loads retry changing totals and reject malformed or repeatedly changing pagination', async () => {
  const item = { page: 1 } as ImportQueueItem
  const queue = vi.spyOn(lastfmGateway, 'queue')
    .mockResolvedValueOnce({ cursor: 0, items: [item], total: 2, nextCursor: 1 })
    .mockResolvedValueOnce({ cursor: 1, items: [item], total: 3, nextCursor: 2 })
    .mockResolvedValueOnce({ cursor: 0, items: [item], total: 1, nextCursor: null })
  expect(await loadImportQueue(() => true)).toEqual([item])
  queue.mockResolvedValue({ cursor: 0, items: [item], total: 2, nextCursor: 0 })
  await expect(loadImportQueue(() => true)).rejects.toThrow('pagination is invalid')
  queue.mockResolvedValue({ cursor: 0, items: [item], total: 2, nextCursor: null })
  await expect(loadImportQueue(() => true)).rejects.toThrow('changed while it was loading')
})
