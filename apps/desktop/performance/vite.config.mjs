import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import { fileURLToPath } from 'node:url'
const native = fileURLToPath(new URL('./native.ts', import.meta.url))
export default defineConfig({
  plugins: [react()],
  resolve: { alias: Object.fromEntries(['core', 'event', 'window'].map(name => [`@tauri-apps/api/${name}`, native])) },
  server: { host: '127.0.0.1', port: 5185, strictPort: true },
})
