import { playwright } from '@vitest/browser-playwright';
import { defineConfig } from 'vitest/config';

const devSlot = Number(process.env.DEV_SLOT ?? 0);
const localPort = process.env.CI ? 0 : 8888 + devSlot * 1000;
const chromiumExecutablePath = process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH;
const e2eApiUrl = process.env.INSTANT_E2E_API_URL ?? '';
const byopAppId = process.env.INSTANT_BYOP_APP_ID ?? '';
const byopMutatorUrl = process.env.INSTANT_BYOP_MUTATOR_URL ?? '';
const byopMutatorToken = process.env.INSTANT_BYOP_MUTATOR_TOKEN ?? '';

export default defineConfig({
  test: {
    projects: [
      {
        extends: true,
        define: {
          __DEV_LOCAL_PORT__: localPort,
          __E2E_API_URL__: JSON.stringify(e2eApiUrl),
          __BYOP_APP_ID__: JSON.stringify(byopAppId),
          __BYOP_MUTATOR_URL__: JSON.stringify(byopMutatorUrl),
          __BYOP_MUTATOR_TOKEN__: JSON.stringify(byopMutatorToken),
        },
        test: {
          name: 'e2e',
          include: ['**/**.e2e.test.ts'],
          expect: {
            poll: {
              timeout: 10_000,
            },
          },
          browser: {
            enabled: true,
            provider: playwright({
              launchOptions: chromiumExecutablePath
                ? { executablePath: chromiumExecutablePath }
                : undefined,
            }),
            screenshotFailures: false,
            headless: true,
            instances: [{ browser: 'chromium' }],
          },
        },
      },
      {
        test: {
          name: 'node',
          include: ['**/**.test.ts'],
          exclude: ['**/**.e2e.test.{ts,js}'],
        },
      },
    ],
  },
});
