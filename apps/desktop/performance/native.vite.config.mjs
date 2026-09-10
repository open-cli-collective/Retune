import { mergeConfig } from 'vite'
import config from '../vite.config.ts'

export default mergeConfig(config, {
  plugins: [{
    name: 'native-performance-probe',
    transformIndexHtml: {
      order: 'pre',
      handler: () => [{ tag: 'script', attrs: { type: 'module', src: '/performance/native-probe.js' }, injectTo: 'head' }],
    },
  }],
})
