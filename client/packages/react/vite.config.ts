import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';
import { resolve } from 'path';

export default defineConfig({
  clearScreen: false,
  plugins: [react()],
  test: {
    environment: 'jsdom',
    include: ['src/tests/**/*.test.ts', 'src/tests/**/*.test.tsx'],
    setupFiles: ['../../scripts/vitest-jsdom-websocket.ts'],
  },
  build: {
    outDir: 'dist/standalone',
    lib: {
      formats: ['umd', 'es'],
      // this is the file that exports our components
      entry: resolve(__dirname, 'src', 'index.ts'),
      name: 'instantReact',
      fileName: 'index',
    },
    rollupOptions: {
      // don't bundle react libs
      // the user will have provided them already
      // via script tags or import maps
      external: ['react', 'react-dom'],
      output: {
        // Provide global variables to use in the UMD build
        // for externalized deps
        globals: {
          react: 'React',
          'react-dom': 'ReactDOM',
        },
      },
    },
  },
  define: {
    'process.env': {},
  },
});
