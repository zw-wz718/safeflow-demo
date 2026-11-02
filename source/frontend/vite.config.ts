import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';
import { fileURLToPath } from 'node:url';

const frontendRoot = fileURLToPath(new URL('.', import.meta.url));
const backendTarget=process.env.SAFEFLOW_BANK_API_TARGET || 'http://127.0.0.1:8787';
if(!/^http:\/\/127\.0\.0\.1:[0-9]{2,5}$/.test(backendTarget))throw Error('loopback backend target required');

export default defineConfig({
  plugins: [react()],
  server: {
    fs: { allow: [frontendRoot] },
    proxy: {
      '/api/bank': {target:backendTarget,changeOrigin:true},
      '/api/demo-verification': {target:backendTarget,changeOrigin:true},
      '/api/team-demo': {target:backendTarget,changeOrigin:true},
      '/api/card-safety': {target:backendTarget,changeOrigin:true},
      '/api/subscriptions': { target: backendTarget, changeOrigin: true },
      '/api/card-read': { target: backendTarget, changeOrigin: true },
      '/api/agent': { target: backendTarget, changeOrigin: true },
      '/api/billing-feedback': { target: backendTarget, changeOrigin: true },
      '/api/sim/payment-preparations': { target: backendTarget, changeOrigin: true },
      '/api/sim/draft-lifecycles': { target: backendTarget, changeOrigin: true },
      '/api/chat': { target: backendTarget, changeOrigin: true },
      '/api/tasks': { target: backendTarget, changeOrigin: true },
      '/api/sim/session': { target: backendTarget, changeOrigin: true },
      '/api/sim/draft-confirmations': { target: backendTarget, changeOrigin: true },
      '/api/sim/draft-executions': { target: backendTarget, changeOrigin: true },
      '/api/operations': { target: backendTarget, changeOrigin: true },
      '/api/llm': { target: backendTarget, changeOrigin: true },
      '/api/health': { target: backendTarget, changeOrigin: true },
    },
  },
  build: {
    rollupOptions: {
      input: {
        workbench: fileURLToPath(new URL('index.html', import.meta.url)),
      },
    },
  },
  test: { include: ['src/**/*.test.ts', 'server/**/*.test.ts'], environment: 'node' },
});
