import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { getCurrentWindow } from '@tauri-apps/api/window'
import './index.css'
// Theme and shared dialogs are used by both windows.
import './App.css'

const isImporter = getCurrentWindow().label === 'lastfm-importer'

const root = createRoot(document.getElementById('root')!)
root.render(<main role="status">Loading Retune…</main>)
const entry = isImporter ? import('./LastFmImporter.tsx') : import('./App.tsx')
entry.then(({ default: WindowView }) => root.render(<StrictMode><WindowView /></StrictMode>))
  .catch(error => {
    console.error('Could not load Retune window', error)
    root.render(<main role="alert">Couldn’t load Retune. <button onClick={() => location.reload()}>Try again</button></main>)
  })
