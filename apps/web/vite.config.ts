import { defineConfig } from 'vite';
import preact from '@preact/preset-vite';
import { VitePWA } from 'vite-plugin-pwa';

export default defineConfig({
  base: '/cascade/',
  plugins: [
    preact(),
    VitePWA({
      registerType: 'autoUpdate',
      includeAssets: ['favicon.svg'],
      manifest: {
        name: 'Cascade',
        short_name: 'Cascade',
        description: 'Cross-platform cloud storage filesystem client',
        theme_color: '#1e40af',
        background_color: '#0f172a',
        display: 'standalone',
        start_url: '/cascade/',
        icons: [
          {
            src: 'favicon.svg',
            sizes: 'any',
            type: 'image/svg+xml',
            purpose: 'any maskable',
          },
        ],
      },
      workbox: {
        globPatterns: ['**/*.{js,css,html,svg,woff2,wasm}'],
        runtimeCaching: [
          {
            urlPattern: ({ url, request }) => url.pathname.startsWith('/v1/') && request.method === 'GET',
            handler: 'NetworkFirst',
            options: {
              cacheName: 'api-cache',
              expiration: {
                maxEntries: 100,
                maxAgeSeconds: 60 * 5,
              },
              networkTimeoutSeconds: 5,
            },
          },
          {
            // Cache Drive API file content downloads for offline access.
            urlPattern: ({ url }) =>
              url.hostname === 'www.googleapis.com'
              && url.pathname.includes('/drive/v3/files/')
              && url.searchParams.get('alt') === 'media',
            handler: 'CacheFirst',
            options: {
              cacheName: 'drive-content-cache',
              expiration: {
                maxEntries: 200,
                maxAgeSeconds: 60 * 60 * 24 * 7, // 7 days
              },
              cacheableResponse: {
                statuses: [0, 200],
              },
            },
          },
        ],
      },
    }),
  ],
  resolve: {
    alias: {
      '@': '/src',
    },
  },
  server: {
    port: 5173,
    proxy: {
      // In dev, proxy /v1 to the local daemon (default port 7842).
      '/v1': {
        target: 'http://localhost:7842',
        changeOrigin: true,
      },
    },
  },
  build: {
    outDir: 'dist',
    target: 'es2022',
  },
});
