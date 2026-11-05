import { fileURLToPath } from 'node:url';
import tailwindcss from '@tailwindcss/vite';
import { tanstackStart } from '@tanstack/react-start/plugin/vite';
import react from '@vitejs/plugin-react';
import { nitro } from 'nitro/vite';
import { defineConfig, loadEnv } from 'vite';
import { publicEnv } from './lib/public-env.mjs';
import { dashboardSecurityHeaders } from './lib/security-headers.mjs';
import virtualRoutes from './virtual-routes.json';

type StartOptions = NonNullable<Parameters<typeof tanstackStart>[0]>;
type VirtualRouteConfig = NonNullable<
  StartOptions['router']
>['virtualRouteConfig'];

const componentsSource = fileURLToPath(
  new URL('../packages/components/src', import.meta.url),
);
const packageSource = (name: string) =>
  fileURLToPath(new URL(`../packages/${name}/src/index.ts`, import.meta.url));

export default defineConfig(({ mode }) => ({
  cacheDir: 'node_modules/.vite-start',
  base: '/_build/',
  define: publicEnv(loadEnv(mode, process.cwd(), ['PUBLIC_', 'NEXT_PUBLIC_'])),
  server: { port: 3002, origin: 'http://localhost:3002' },
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('.', import.meta.url)),
      '@instantdb/components': componentsSource,
      '@instantdb/admin': packageSource('admin'),
      '@instantdb/core': packageSource('core'),
      '@instantdb/platform': packageSource('platform'),
      '@instantdb/react': packageSource('react'),
      '@lib': componentsSource,
    },
  },
  optimizeDeps: {
    esbuildOptions: {
      loader: { '.js': 'jsx' },
    },
  },
  ssr: {
    noExternal: ['@uiw/react-json-view', 'lodash'],
  },
  plugins: [
    // Import the route config so Vite watches it and restarts the generator on changes.
    tanstackStart({
      router: {
        virtualRouteConfig: virtualRoutes as VirtualRouteConfig,
        basepath: '/',
      },
    }),
    nitro({ routeRules: { '/**': { headers: dashboardSecurityHeaders(mode === 'development') } } }),
    tailwindcss(),
    react(),
  ],
}));
