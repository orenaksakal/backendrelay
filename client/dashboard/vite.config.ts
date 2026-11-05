import { tanstackStart } from '@tanstack/react-start/plugin/vite';
import tailwindcss from '@tailwindcss/vite';
import react from '@vitejs/plugin-react';
import { msw } from 'msw/vite';
import { fileURLToPath } from 'node:url';
import { defineConfig, type PluginOption } from 'vite';

// Mock mode reuses the Vitest handlers in the dev server and in the e2e build.
// `msw/vite` registers the service worker and bundles the handler module, so
// production builds neither ship the mocks nor start a worker.
const mockApiEnabled = process.env.VITE_MOCK_API === '1';

// The data screens need an Instant socket. A static mock build has none, so a
// spec that opts in (`localStorage["br.mockInstant"]`) gets an in-memory client
// instead; every other spec keeps the inert socket it already asserts against.
const instantStub = fileURLToPath(
  new URL('./src/test/stubs/instant-react.ts', import.meta.url),
);

function mockApiPlugins(): PluginOption[] {
  if (!mockApiEnabled) return [];
  return [msw({ mode: 'worker-only' })];
}

export default defineConfig({
  // TanStack Start starts a Vite preview server for prerendering and fetches
  // it over localhost. Bind the preview server to IPv4 explicitly: in a
  // container localhost resolves to ::1 first, which can leave the prerender
  // fetch with ECONNREFUSED when the server is IPv4-only.
  preview: {
    host: '127.0.0.1',
  },
  resolve: {
    alias: {
      '@': new URL('./src', import.meta.url).pathname,
      ...(mockApiEnabled ? { '@instantdb/react': instantStub } : {}),
    },
  },
  // SPA mode prerenders one static HTML shell (`/_shell.html`) plus hashed
  // assets, so the build needs no Node server at runtime.
  plugins: [
    tanstackStart({ spa: { enabled: true } }),
    react(),
    tailwindcss(),
    ...mockApiPlugins(),
  ],
});
