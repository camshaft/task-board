import tailwindcss from '@tailwindcss/vite'
import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'

// The Rust backend serves /api (REST) and /mcp (MCP). In dev we proxy both to it so
// the UI can use same-origin relative URLs; in prod the backend serves the built assets.
const BACKEND = process.env.TB_BACKEND ?? 'http://localhost:8079'

export default defineConfig({
  // Relative asset URLs (./assets/…) so a single build works at any mount point. When
  // the app is served under a sub-path, the Rust backend injects a matching <base href>
  // (from the X-Forwarded-Prefix header) at serve time; nothing is baked in at build.
  base: './',
  plugins: [react(), tailwindcss()],
  server: {
    proxy: {
      '/api': { target: BACKEND, changeOrigin: true },
      '/mcp': { target: BACKEND, changeOrigin: true },
    },
  },
  build: { outDir: 'dist' },
})
