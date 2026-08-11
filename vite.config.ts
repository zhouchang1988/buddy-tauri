import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import { fileURLToPath, URL } from 'node:url'

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url))
    }
  },
  // Tauri expects a fixed port in dev
  server: {
    port: 1420,
    strictPort: true
  },
  build: {
    outDir: 'dist',
    target: 'es2021'
  },
  clearScreen: false,
  test: {
    environment: 'jsdom',
    globals: true,
    setupFiles: ['tests/setup.ts'],
    css: false
  }
})
