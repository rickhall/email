import { defineConfig } from 'vite';
import solid from 'vite-plugin-solid';

// Avoid pulling in @types/node just for env access.
declare const process: { env: Record<string, string | undefined> };

const host = process.env.TAURI_DEV_HOST;

// Tauri v2 conventions: https://v2.tauri.app/start/frontend/vite/
export default defineConfig({
  plugins: [solid()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host ? { protocol: 'ws', host, port: 1421 } : undefined,
    watch: { ignored: ['**/src-tauri/**'] },
  },
  envPrefix: ['VITE_', 'TAURI_ENV_'],
  build: {
    // Tauri uses WebKit on macOS and Linux.
    target: 'safari15',
    minify: !process.env.TAURI_ENV_DEBUG,
    sourcemap: !!process.env.TAURI_ENV_DEBUG,
  },
});
