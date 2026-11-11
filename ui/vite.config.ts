import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

const backend = 'http://127.0.0.1:3000';

// For `npm run dev`, start the server with SOURCELENS_DEV_ORIGIN=http://127.0.0.1:5173.
export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      '/api': backend,
      '/auth': backend,
      '/mcp': backend,
      '/webhooks': backend,
    },
  },
});
