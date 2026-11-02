// Isolated verification server: an explicit task-owned API is required.
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
const target = process.env.SAFEFLOW_RECOVERY_API;
if (!target || !/^http:\/\/127\.0\.0\.1:\d+$/.test(target))
  throw new Error('explicit loopback recovery test API required');
export default defineConfig({
  plugins: [react()],
  server: {
    host: '127.0.0.1',
    strictPort: true,
    proxy: { '/api': { target, changeOrigin: true } },
  },
});
