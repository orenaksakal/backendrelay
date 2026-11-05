import { fileURLToPath } from 'node:url';
import react from '@vitejs/plugin-react';
import { defineConfig } from 'vitest/config';

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
      'react-resizable-panels': fileURLToPath(
        new URL('./src/test/stubs/resizable-panels.tsx', import.meta.url),
      ),
    },
  },
  test: {
    environment: 'jsdom',
    environmentOptions: {
      jsdom: {
        // MSW's relative handlers and the runtime config share this origin.
        url: 'http://localhost:8888',
      },
    },
    setupFiles: ['./src/test/setup.ts'],
    include: ['src/**/*.test.{ts,tsx}'],
    exclude: ['e2e/**', 'node_modules/**', 'dist/**'],
    restoreMocks: true,
    css: false,
  },
});
