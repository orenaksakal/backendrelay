import js from '@eslint/js';
import tseslint from 'typescript-eslint';
import reactHooks from 'eslint-plugin-react-hooks';
import { reactRefresh } from 'eslint-plugin-react-refresh';
import jsxA11y from 'eslint-plugin-jsx-a11y';
import pluginQuery from '@tanstack/eslint-plugin-query';
import pluginRouter from '@tanstack/eslint-plugin-router';
import prettier from 'eslint-config-prettier';
import globals from 'globals';
import { defineConfig, globalIgnores } from 'eslint/config';

export default defineConfig([
  globalIgnores([
    'dist',
    '.vitest',
    'playwright-report',
    'test-results',
    'src/routeTree.gen.ts',
  ]),
  {
    files: ['**/*.{ts,tsx}'],
    extends: [
      js.configs.recommended,
      tseslint.configs.strictTypeChecked,
      reactHooks.configs.flat.recommended,
      reactRefresh.configs.vite(),
      jsxA11y.flatConfigs.strict,
      pluginQuery.configs['flat/recommended'],
      pluginRouter.configs['flat/recommended'],
    ],
    languageOptions: {
      globals: globals.browser,
      parserOptions: {
        projectService: true,
        tsconfigRootDir: import.meta.dirname,
      },
    },
    settings: {
      'jsx-a11y': {
        components: {
          Button: 'button',
          Input: 'input',
          Textarea: 'textarea',
          Label: 'label',
        },
      },
    },
    rules: {
      'react-refresh/only-export-components': [
        'warn',
        { allowExportNames: ['Route'] },
      ],
      'no-restricted-imports': [
        'error',
        {
          patterns: [
            {
              group: [
                '@radix-ui/*',
                'radix-ui',
                '@headlessui/*',
                '@heroicons/*',
                'sonner',
                'next-themes',
                '@monaco-editor/*',
              ],
              message: 'Use @/components/ui or the documented wrappers.',
            },
          ],
        },
      ],
    },
  },
  {
    files: ['src/components/ui/**'],
    rules: { 'react-refresh/only-export-components': 'off' },
  },
  {
    // TanStack Router signals redirects and not-found states by throwing
    // response objects rather than Error instances.
    files: ['src/routes/**/*.tsx', 'src/lib/workspace/app-loader.ts'],
    rules: { '@typescript-eslint/only-throw-error': 'off' },
  },
  prettier,
]);
