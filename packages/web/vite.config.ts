import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { readFileSync } from 'node:fs';
import { VitePWA } from 'vite-plugin-pwa';
import { themeStoragePlugin } from '../../vite-theme-plugin';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const packageJson = JSON.parse(readFileSync(path.resolve(__dirname, 'package.json'), 'utf-8'));
const pwaDevEnabled = process.env.OPENCHAMBER_DISABLE_PWA_DEV !== '1';
const reactScanToggle = (process.env.VITE_ENABLE_REACT_SCAN ?? '').toLowerCase();
const enableReactScan = reactScanToggle === '1' || reactScanToggle === 'true' || reactScanToggle === 'on' || reactScanToggle === 'yes';

export default defineConfig({
  root: path.resolve(__dirname, '.'),
  plugins: [
    react({
      babel: {
        plugins: ['babel-plugin-react-compiler'],
      },
    }),
    {
      name: 'inject-react-scan-script',
      transformIndexHtml() {
        if (!enableReactScan) {
          return;
        }
        return [
          {
            tag: 'script',
            attrs: {
              crossorigin: 'anonymous',
              src: '//unpkg.com/react-scan/dist/auto.global.js',
            },
            injectTo: 'head-prepend',
          },
        ];
      },
    },
    {
      // 在 index.html 注入 API base URL (在 main.tsx 加载前执行)。
      // Tauri dev 模式: GRIDFORGE_PORT=3001 → __OPENCHAMBER_API_BASE_URL__="http://127.0.0.1:3001"
      // 使得 WS 直连 oc-server 不走 Vite proxy, 避免 ECONNRESET。
      // 比 Tauri 的 window.eval() 更可靠: 脚本同步执行于 HTML 解析阶段。
      // OPENCHAMBER_PORT 保留为回退兼容旧用户脚本。
      name: 'inject-api-base-url',
      transformIndexHtml() {
        const apiPort = process.env.GRIDFORGE_PORT ?? process.env.OPENCHAMBER_PORT ?? '3001';
        const origin = `http://127.0.0.1:${apiPort}`;
        return [
          {
            tag: 'script',
            attrs: { type: 'text/javascript' },
            children: [
              `window.__OPENCHAMBER_API_BASE_URL__=${JSON.stringify(origin)};`,
              `window.__OPENCHAMBER_LOCAL_ORIGIN__=${JSON.stringify(origin)};`,
            ].join('\n'),
            injectTo: 'head',
          },
        ];
      },
    },
    themeStoragePlugin(),
    VitePWA({
      strategies: 'injectManifest',
      srcDir: 'src',
      filename: 'sw.ts',
      registerType: 'autoUpdate',
      injectRegister: false,
      manifest: false,
      injectManifest: {
        globPatterns: ['**/*.{js,css,html,ico,png,svg,woff,woff2,ttf,otf,eot}'],
        // iOS Safari/PWA is much more reliable with a classic (non-module) SW bundle.
        rollupFormat: 'iife',
        // We already keep a custom manifest in index.html
        injectionPoint: undefined,
      },
      devOptions: {
        enabled: pwaDevEnabled,
        type: 'module',
      },
    }),
  ],
  resolve: {
    alias: [
      { find: '@opencode-ai/sdk/v2', replacement: path.resolve(__dirname, '../../node_modules/@opencode-ai/sdk/dist/v2/client.js') },
      { find: '@openchamber/ui', replacement: path.resolve(__dirname, '../ui/src') },
      { find: '@web', replacement: path.resolve(__dirname, './src') },
      { find: '@', replacement: path.resolve(__dirname, '../ui/src') },
    ],
  },
  worker: {
    format: 'es',
  },
  define: {
    'process.env': {},
    global: 'globalThis',
    __APP_VERSION__: JSON.stringify(packageJson.version),
  },
  optimizeDeps: {
    include: ['@opencode-ai/sdk/v2'],
  },
  server: {
    port: 5173,
    proxy: {
      '/auth': {
        target: `http://127.0.0.1:${process.env.GRIDFORGE_PORT ?? process.env.OPENCHAMBER_PORT ?? 3001}`,
        changeOrigin: true,
      },
      '/health': {
        target: `http://127.0.0.1:${process.env.GRIDFORGE_PORT ?? process.env.OPENCHAMBER_PORT ?? 3001}`,
        changeOrigin: true,
      },
      '/api': {
        target: `http://127.0.0.1:${process.env.GRIDFORGE_PORT ?? process.env.OPENCHAMBER_PORT ?? 3001}`,
        changeOrigin: true,
        ws: true,
      },
    },
  },
  build: {
    outDir: path.resolve(__dirname, 'dist'),
    emptyOutDir: true,
    chunkSizeWarningLimit: 500,
    rollupOptions: {
      input: {
        main: path.resolve(__dirname, 'index.html'),
        mobile: path.resolve(__dirname, 'mobile.html'),
        miniChat: path.resolve(__dirname, 'mini-chat.html'),
      },
      external: ['node:child_process', 'node:fs', 'node:path', 'node:url'],
      output: {
        manualChunks(id) {
          if (!id.includes('node_modules')) return undefined;

          const match = id.split('node_modules/')[1];
          if (!match) return undefined;

          const segments = match.split('/');
          const packageName = match.startsWith('@') ? `${segments[0]}/${segments[1]}` : segments[0];

          if (packageName === 'react' || packageName === 'react-dom') return 'vendor-react';
          if (packageName === 'zustand' || packageName === 'zustand/middleware') return 'vendor-zustand';

          if (packageName === '@opencode-ai/sdk') return 'vendor-opencode-sdk';
          if (packageName.includes('remark') || packageName.includes('rehype') || packageName === 'react-markdown') return 'vendor-markdown';
          if (packageName === '@base-ui/react' || packageName.startsWith('@base-ui')) return 'vendor-base-ui';
          if (packageName.includes('react-syntax-highlighter') || packageName.includes('highlight.js')) return 'vendor-syntax';

          const sanitized = packageName.replace(/^@/, '').replace(/\//g, '-');
          return `vendor-${sanitized}`;
        },
      },
    },
  },
});
