import tailwindcss from '@tailwindcss/vite'
import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'

// The Rust backend serves /api (REST) and /mcp (MCP). In dev we proxy both to it so
// the UI can use same-origin relative URLs; in prod the backend serves the built assets.
const BACKEND = process.env.TB_BACKEND ?? 'http://localhost:8079'

// Public base path the app is served under. Defaults to '/'. Set VITE_BASE_PATH (e.g.
// '/board') when the app sits behind a reverse proxy on a sub-path so every asset and
// API URL is emitted with that prefix. Vite normalizes it to have a trailing slash and
// exposes it as import.meta.env.BASE_URL, which api.ts uses to build request URLs.
const BASE = process.env.VITE_BASE_PATH ?? '/'

export default defineConfig({
  base: BASE,
  plugins: [react(), tailwindcss()],
  server: {
    proxy: {
      '/api': { target: BACKEND, changeOrigin: true },
      '/mcp': { target: BACKEND, changeOrigin: true },
    },
  },
  build: { outDir: 'dist' },
})
