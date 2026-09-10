import { Profiler } from 'react'
import { createRoot } from 'react-dom/client'
import LastFmImporter from '../src/LastFmImporter.tsx'
import { invalidate, queueWire } from './native.ts'
import '../src/index.css'
import '../src/App.css'
const wait = (ms: number) => new Promise(resolve => setTimeout(resolve, ms))
let measuring = false, renderMs = 0, commits = 0
const results: unknown[] = []
const panel = document.createElement('aside')
panel.style.cssText = 'position:fixed;bottom:0;right:0;z-index:10000;background:white;color:black;max-height:160px;overflow:auto;font:12px monospace'
const button = document.createElement('button'); button.textContent = 'Measure importer refresh burst'
const output = document.createElement('pre'); output.id = 'importer-results'
panel.append(button, output); document.body.append(panel)
button.onclick = async () => {
  button.disabled = true
  const calls = queueWire.calls, bytes = queueWire.bytes
  renderMs = 0; commits = 0; measuring = true
  const start = performance.now()
  for (let i = 0; i < 6; i++) { invalidate('lastfm-import-changed'); await wait(1) }
  while (queueWire.inFlight) await wait(1)
  await new Promise(requestAnimationFrame); await new Promise(requestAnimationFrame)
  measuring = false
  results.push({ durationMs: performance.now() - start, queueCalls: queueWire.calls - calls, queueBytes: queueWire.bytes - bytes, renderMs, commits, queueRows: document.querySelectorAll('[data-import-nav="queue"]').length, selectionLabel: document.querySelector('.import-queue-bulk-actions')?.textContent, viewport: [innerWidth,innerHeight] })
  output.textContent = JSON.stringify(results, null, 2); button.disabled = false
}
createRoot(document.getElementById('root')!).render(<Profiler id="importer" onRender={(_,__,duration) => { if(measuring) { renderMs += duration; commits++ } }}><LastFmImporter /></Profiler>)
