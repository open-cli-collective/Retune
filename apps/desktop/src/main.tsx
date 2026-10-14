import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { getCurrentWindow } from '@tauri-apps/api/window'
import './index.css'
// Theme and shared dialogs are used by every window.
import './App.css'

const label = getCurrentWindow().label

const root = createRoot(document.getElementById('root')!)
root.render(<main role="status">Loading Retune…</main>)
// Keep separate loader functions so Vite associates each chunk with its own CSS.
const entries = { main: () => import('./App.tsx'), importer: () => import('./LastFmImporter.tsx'), preferences: () => import('./PreferencesWindow.tsx') }
entries[label === 'lastfm-importer' ? 'importer' : label === 'preferences' ? 'preferences' : 'main']().then(({ default: WindowView }) => root.render(<StrictMode><WindowView /></StrictMode>))
  .catch(error => {
    console.error('Could not load Retune window', error)
    root.render(<main role="alert">Couldn’t load Retune. <button onClick={() => location.reload()}>Try again</button></main>)
  })
