import { Profiler } from 'react'
import { createRoot } from 'react-dom/client'
import App from '../src/App.tsx'
import '../src/index.css'
import { calls, emit, player } from './native.ts'

const results: unknown[] = []
let measuring = false
let renders: number[] = []
let frames: number[] = []
let longTasks: number[] = []
let previousFrame = 0
function frame(now: number) {
  if (measuring && previousFrame) frames.push(now - previousFrame)
  previousFrame = now
  requestAnimationFrame(frame)
}
requestAnimationFrame(frame)
if (PerformanceObserver.supportedEntryTypes.includes('longtask')) {
  new PerformanceObserver(list => { if (measuring) longTasks.push(...list.getEntries().map(entry => entry.duration)) }).observe({ type: 'longtask' })
}
const panel = document.createElement('aside')
panel.style.cssText = 'position:fixed;right:12px;bottom:26px;z-index:10000;background:white;color:black;border:1px solid #888;padding:8px;font:12px monospace;max-width:520px;max-height:180px;overflow:auto'
const button = document.createElement('button')
button.textContent = 'Measure 12 playback ticks'
const output = document.createElement('pre'); output.id = 'performance-results'
panel.append(button, output); document.body.append(panel)
const wait = (milliseconds: number) => new Promise(resolve => setTimeout(resolve, milliseconds))
button.onclick = async () => {
  button.disabled = true
  const first = document.querySelector<HTMLElement>('[data-track-id], [data-upstream-index]')
  first?.dispatchEvent(new MouseEvent('dblclick', { bubbles: true }))
  emit({ type: 'playerState', payload: player(0) })
  await wait(1500)
  renders = []; frames = []; longTasks = []
  const callsBefore = calls.length
  measuring = true
  const started = performance.now()
  for (let elapsed = 1; elapsed <= 12; elapsed++) { emit({ type: 'playerState', payload: player(elapsed) }); await wait(1000) }
  measuring = false
  const sample = {
    scenario: document.querySelector('.playlist-view') ? 'playlist-playback' : 'library-playback',
    durationMs: performance.now() - started, commits: renders.length,
    totalRenderMs: renders.reduce((sum, duration) => sum + duration, 0), maxRenderMs: Math.max(0, ...renders),
    maxFrameGapMs: Math.max(0, ...frames), longTasksMs: longTasks,
    ipcDuringTicks: calls.length - callsBefore, rows: document.querySelectorAll('[data-track-id], .playlist-track-row').length,
    elapsed: document.querySelector<HTMLInputElement>('[aria-label="Playback position"]')?.value,
    viewport: [innerWidth, innerHeight], samples: renders,
  }
  results.push(sample); output.textContent = JSON.stringify(results, null, 2); button.disabled = false
}
createRoot(document.getElementById('root')!).render(<Profiler id="app" onRender={(_, __, duration) => { if (measuring) renders.push(duration) }}><App /></Profiler>)
