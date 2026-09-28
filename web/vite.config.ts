import tailwindcss from '@tailwindcss/vite'
import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'

// The Rust backend serves /api (REST) and /mcp (MCP). In dev we proxy both to it so
// the UI can use same-origin relative URLs; in prod the backend serves the built assets.
const BACKEND = process.env.TB_BACKEND ?? 'http://localhost:8079'

export default defineConfig({
  plugins: [react(), tailwindcss()],
  server: {
    proxy: {
      '/api': { target: BACKEND, changeOrigin: true },
      '/mcp': { target: BACKEND, changeOrigin: true },
    },
  },
  build: { outDir: 'dist' },
})
