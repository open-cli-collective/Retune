// @vitest-environment jsdom
import { beforeEach, expect, it, vi } from 'vitest'

const entry = vi.hoisted(() => ({ label: 'main', fail: false, render: vi.fn(), main: vi.fn(), importer: vi.fn() }))
vi.mock('@tauri-apps/api/window', () => ({ getCurrentWindow: () => ({ label: entry.label }) }))
vi.mock('react-dom/client', () => ({ createRoot: () => ({ render: entry.render }) }))
beforeEach(() => {
  vi.resetModules(); vi.clearAllMocks(); entry.fail = false
  vi.doMock('../src/App.tsx', () => {
    entry.main()
    if (entry.fail) throw new Error('missing main chunk')
    return { default: () => null }
  })
  vi.doMock('../src/LastFmImporter.tsx', () => { entry.importer(); return { default: () => null } })
})

it.each(['main', 'lastfm-importer'])('loads only the selected %s window entry', async label => {
  entry.label = label
  await import('../src/main.tsx')
  await vi.dynamicImportSettled()
  expect(entry.main).toHaveBeenCalledTimes(label === 'main' ? 1 : 0)
  expect(entry.importer).toHaveBeenCalledTimes(label === 'lastfm-importer' ? 1 : 0)
  expect(entry.render.mock.calls[0][0].props.role).toBe('status')
  expect(entry.render.mock.calls.at(-1)![0].props.children.type).toBeTypeOf('function')
})

it('replaces loading with an accessible retry when the selected chunk fails', async () => {
  entry.label = 'main'; entry.fail = true
  const error = vi.spyOn(console, 'error').mockImplementation(() => {})
  try {
    await import('../src/main.tsx')
    await vi.dynamicImportSettled()
    const result = entry.render.mock.calls.at(-1)![0]
    expect(result.props.role).toBe('alert')
    expect(result.props.children[1].props.children).toBe('Try again')
    expect(entry.importer).not.toHaveBeenCalled()
  } finally { error.mockRestore() }
})
