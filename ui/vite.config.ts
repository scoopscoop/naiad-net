/// <reference types="vitest/config" />
import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import { svelteTesting } from '@testing-library/svelte/vite';

// Dev: proxy the daemon's API/media routes so the browser sees one origin
// (no CORS needed). Prod: the daemon serves the built `dist/` itself.
export default defineConfig({
  plugins: [svelte(), svelteTesting()],
  build: {
    // Two entries: the app, and the standalone page the Tauri shell opens at
    // launch while the daemon starts (#48). `loading.html` is bundled into the
    // desktop binary via `frontendDist`; the daemon's rust-embed copy of dist/
    // also carries it, where it is simply an unused route.
    rollupOptions: {
      input: {
        main: 'index.html',
        loading: 'loading.html',
      },
    },
  },
  server: {
    proxy: {
      '/api': 'http://127.0.0.1:8080',
      '/thumb': { target: 'http://127.0.0.1:8080', ws: true },
      '/file': 'http://127.0.0.1:8080',
    },
  },
  test: {
    environment: 'jsdom',
    globals: true,
    setupFiles: ['./src/test-setup.ts'],
  },
});
