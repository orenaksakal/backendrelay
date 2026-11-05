import { defineConfig, devices } from '@playwright/test';

const PORT = 4173;
const baseURL = `http://127.0.0.1:${String(PORT)}`;

/**
 * The live suite runs against a real server started by
 * `self-hosting/run-dashboard-browser-acceptance.sh`: the script builds the
 * dashboard, serves it with `vite preview` and points it at the Compose stack
 * through the generated `/config.json`. Selecting `BR_LIVE=1` swaps the mocked
 * projects for that one journey (D9.2).
 */
const live = process.env.BR_LIVE === '1';
const liveBaseURL = process.env.BR_LIVE_DASHBOARD_URL ?? baseURL;

const mockedProjects = [
  {
    name: 'chromium-light',
    testIgnore: '**/live/**',
    use: { ...devices['Desktop Chrome'], colorScheme: 'light' as const },
  },
  {
    name: 'chromium-dark',
    testIgnore: '**/live/**',
    use: { ...devices['Desktop Chrome'], colorScheme: 'dark' as const },
  },
  {
    name: 'firefox-smoke',
    testIgnore: '**/live/**',
    use: { ...devices['Desktop Firefox'] },
    grep: /@smoke/,
  },
  {
    name: 'webkit-smoke',
    testIgnore: '**/live/**',
    use: { ...devices['Desktop Safari'] },
    grep: /@smoke/,
  },
];

export default defineConfig({
  testDir: './e2e',
  fullyParallel: true,
  forbidOnly: Boolean(process.env.CI),
  retries: process.env.CI ? 2 : 0,
  workers: process.env.CI ? 1 : undefined,
  reporter: process.env.CI
    ? [['github'], ['html', { open: 'never' }]]
    : [['list'], ['html', { open: 'never' }]],
  use: {
    baseURL,
    reducedMotion: 'reduce',
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
    video: 'retain-on-failure',
  },
  expect: {
    toHaveScreenshot: {
      animations: 'disabled',
      caret: 'hide',
      maxDiffPixelRatio: 0.01,
    },
  },
  projects: live
    ? [
        {
          name: 'live',
          testDir: './e2e/live',
          fullyParallel: false,
          workers: 1,
          timeout: 300_000,
          use: {
            ...devices['Desktop Chrome'],
            baseURL: liveBaseURL,
            colorScheme: 'light',
          },
        },
      ]
    : mockedProjects,
  // The script owns the server and the real build for the live project.
  webServer: live
    ? undefined
    : {
        command: 'pnpm build:e2e && pnpm preview --port 4173 --strictPort',
        url: baseURL,
        reuseExistingServer: !process.env.CI,
        timeout: 180_000,
        stdout: 'pipe',
        stderr: 'pipe',
      },
});
