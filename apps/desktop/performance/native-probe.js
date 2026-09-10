// Opt-in bundled harness: actual App, IPC and WebKit; no production entry changes.
const wait = ms => new Promise(resolve => setTimeout(resolve, ms))
const painted = async () => { await new Promise(requestAnimationFrame); await new Promise(requestAnimationFrame) }
const until = async predicate => {
  const start = performance.now()
  while (!predicate()) {
    if (performance.now() - start > 15000) throw new Error('Native fixture timed out')
    await wait(10)
  }
}
const results = { navigationTimeOrigin: performance.timeOrigin, readyMs: null, interactions: [], visibility: [], playback: [] }
const panel = document.createElement('aside')
panel.style.cssText = 'position:fixed;right:12px;bottom:26px;z-index:10000;background:white;color:black;border:1px solid #888;padding:6px;font:12px monospace'
const output = document.createElement('textarea')
output.readOnly = true
output.setAttribute('aria-label', 'Native performance results')
output.style.cssText = 'display:block;width:380px;height:90px;font:10px monospace'
const show = () => { output.value = JSON.stringify(results) }
const snapshot = () => ({ atMs: performance.now(), state: document.visibilityState,
  elapsed: document.querySelector('[aria-label="Playback position"]')?.value,
  animationStates: [...document.querySelectorAll('.marquee')].flatMap(node => node.getAnimations().map(animation => animation.playState)) })
document.addEventListener('visibilitychange', () => {
  results.visibility.push(snapshot()); show()
  setTimeout(() => { results.visibility.push(snapshot()); show() }, 100)
})
const button = (label, action) => {
  const control = document.createElement('button')
  control.textContent = label
  control.onclick = async () => {
    control.disabled = true
    try { await action() } catch (error) { results.error = String(error) }
    finally { control.disabled = false; show() }
  }
  panel.append(control)
}
button('Measure native playlist', async () => {
  for (let sample = 0; sample < 3; sample++) {
    document.querySelector('.source-row').click()
    await until(() => document.querySelector('[data-track-id]'))
    await wait(300)
    const actions = []
    const measure = async (name, action, ready = () => true) => {
      const start = performance.now()
      action(); await until(ready); await painted()
      actions.push({ name, durationMs: performance.now() - start,
        mountedRows: document.querySelectorAll('.playlist-track-row').length,
        selectedRows: document.querySelectorAll('.playlist-track-row.selected').length,
        focusedRow: document.activeElement?.getAttribute('aria-label') })
    }
    await measure('open', () => document.querySelector('.playlist-row').click(), () => document.querySelector('.playlist-track-row'))
    await measure('select', () => document.querySelector('.playlist-track-row').click())
    await measure('end', () => document.querySelector('.playlist-track-row').dispatchEvent(new KeyboardEvent('keydown', { key: 'End', bubbles: true })))
    await measure('shift-home', () => document.activeElement.dispatchEvent(new KeyboardEvent('keydown', { key: 'Home', shiftKey: true, bubbles: true })))
    await measure('sort', () => document.querySelector('.playlist-track-header [data-column="name"]').click())
    await measure('scroll-middle', () => { const scroll = document.querySelector('.playlist-track-scroll'); scroll.scrollTop = scroll.scrollHeight / 2 })
    results.interactions.push({ viewport: [innerWidth, innerHeight], actions }); show()
  }
})
button('Five-second silent playback', async () => {
  try {
    document.querySelector('.source-row').click()
    await until(() => document.querySelector('[data-track-id]'))
    document.querySelector('[data-track-id]').dispatchEvent(new MouseEvent('dblclick', { bubbles: true }))
    await until(() => document.querySelector('[aria-label="Pause"]'))
    results.playback.push(snapshot())
    await wait(5000)
    results.playback.push(snapshot())
  } finally { document.querySelector('[aria-label="Pause"]')?.click() }
})
panel.append(output)
document.body.append(panel)
until(() => document.querySelector('[data-track-id]')).then(painted).then(() => {
  results.readyMs = performance.now(); show()
}).catch(error => { results.error = String(error); show() })
