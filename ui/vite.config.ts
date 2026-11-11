import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

const backend = 'http://127.0.0.1:3000';

// For `npm run dev`, start the server with SOURCELENS_DEV_ORIGIN=http://127.0.0.1:5173.
// The browser's Origin header passes through the proxy unchanged; the server accepts
// non-GET /api requests only from that exact origin, so the port must not drift.
export default defineConfig({
  plugins: [react()],
  server: {
    host: '127.0.0.1',
    port: 5173,
    strictPort: true,
    proxy: {
      '/api': backend,
      '/auth': backend,
      '/mcp': backend,
      '/webhooks': backend,
    },
  },
});
