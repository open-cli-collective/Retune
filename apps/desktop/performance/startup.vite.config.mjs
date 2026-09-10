import { defineConfig, mergeConfig } from 'vite'
import config from './vite.config.mjs'
export default mergeConfig(config, defineConfig({
  build: { outDir: 'performance-dist', manifest: true, rollupOptions: { input: 'performance/startup.html' } },
  preview: { host: '127.0.0.1', port: 5186, strictPort: true },
}))
