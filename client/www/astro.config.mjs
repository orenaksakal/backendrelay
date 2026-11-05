import { defineConfig, passthroughImageService } from 'astro/config';
import node from '@astrojs/node';
import react from '@astrojs/react';
import tailwindcss from '@tailwindcss/vite';
import { loadEnv, transformWithEsbuild } from 'vite';
import { publicEnv } from './lib/public-env.mjs';

const wwwRoot = new URL('./', import.meta.url).pathname;

export default defineConfig({
  srcDir: './site',
  publicDir: './public',
  outDir: './dist-site',
  output: 'server',
  adapter: node({ mode: 'standalone' }),
  integrations: [react()],
  // WebsiteImage serves ordinary <img> URLs; native image optimization is unused.
  image: {
    service: passthroughImageService(),
    endpoint: { entrypoint: './site/image-disabled.ts' },
  },
  vite: {
    cacheDir: 'node_modules/.vite-astro',
    define: publicEnv({
      ...loadEnv(process.env.NODE_ENV || 'development', process.cwd(), [
        'PUBLIC_',
        'NEXT_PUBLIC_',
      ]),
      ...process.env,
    }),
    plugins: [
      tailwindcss(),
      {
        name: 'instant-legacy-jsx',
        enforce: 'pre',
        transform(code, id) {
          if (!id.startsWith(wwwRoot) || !id.endsWith('.js')) return;
          return transformWithEsbuild(code, id, {
            loader: 'jsx',
            jsx: 'automatic',
          });
        },
      },
    ],
    optimizeDeps: {
      esbuildOptions: {
        loader: { '.js': 'jsx' },
      },
    },
    ssr: {
      noExternal: ['lodash', 'prism-react-renderer'],
    },
    resolve: {
      alias: [
        { find: /^@\//, replacement: wwwRoot },
        {
          find: '@lib',
          replacement: new URL('../packages/components/src', import.meta.url)
            .pathname,
        },
        {
          find: '@instantdb/admin',
          replacement: new URL(
            '../packages/admin/src/index.ts',
            import.meta.url,
          ).pathname,
        },
        {
          find: '@instantdb/components',
          replacement: new URL(
            '../packages/components/src/index.tsx',
            import.meta.url,
          ).pathname,
        },
        {
          find: '@instantdb/core',
          replacement: new URL('../packages/core/src/index.ts', import.meta.url)
            .pathname,
        },
        {
          find: '@instantdb/platform',
          replacement: new URL(
            '../packages/platform/src/index.ts',
            import.meta.url,
          ).pathname,
        },
        {
          find: '@instantdb/react',
          replacement: new URL(
            '../packages/react/src/index.ts',
            import.meta.url,
          ).pathname,
        },
        {
          find: '@instantdb/react-common',
          replacement: new URL(
            '../packages/react-common/src/index.ts',
            import.meta.url,
          ).pathname,
        },
        {
          find: '@instantdb/version',
          replacement: new URL(
            '../packages/version/src/index.ts',
            import.meta.url,
          ).pathname,
        },
        {
          find: '@instantdb/webhooks',
          replacement: new URL(
            '../packages/webhooks/src/index.ts',
            import.meta.url,
          ).pathname,
        },
      ],
    },
  },
});
