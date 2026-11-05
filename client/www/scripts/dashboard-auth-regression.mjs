import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const engines = require('../../packages/core/node_modules/playwright');
const browser = await engines[
  process.env.INSTANT_DASHBOARD_BROWSER || 'chromium'
].launch({ headless: true });
const dashboardUrl = new URL(
  '/dash',
  process.env.BASE_URL ?? 'http://localhost:3000',
);
const token = '00000000-0000-0000-0000-000000000000';

try {
  for (const scenario of [
    {
      name: 'missing dashboard user',
      status: 400,
      record: 'instant-user',
      logout: true,
    },
    { name: 'unauthorized', status: 401, record: 'instant-user', logout: true },
    { name: 'missing app', status: 400, record: 'app', logout: false },
    ...(process.env.LIVE_API
      ? [{ name: 'live invalid session', logout: true }]
      : []),
  ]) {
    const context = await browser.newContext();
    let page;
    try {
      page = await context.newPage();
      page.on('pageerror', (error) => console.error(error.message));
      await page.addInitScript((token) => {
        if (!localStorage.getItem('auth-test-initialized')) {
          localStorage.setItem('devBackend', 'true');
          localStorage.setItem('@AUTH', JSON.stringify({ token }));
          localStorage.setItem('auth-test-initialized', 'true');
        }
      }, token);
      if (scenario.status) {
        await page.route('**/dash', async (route) => {
          if (route.request().isNavigationRequest()) return route.continue();
          await route.fulfill({
            status: scenario.status,
            contentType: 'application/json',
            body: JSON.stringify({
              type: 'record-not-found',
              message: `Record not found: ${scenario.record}`,
              hint: { 'record-type': scenario.record },
            }),
          });
        });
      }
      await page.goto(dashboardUrl.toString());
      if (scenario.logout) {
        await page
          .getByRole('heading', { name: 'Sign in to your workspace' })
          .waitFor();
        assert.equal(
          await page.evaluate(
            () => JSON.parse(localStorage.getItem('@AUTH')).token,
          ),
          undefined,
        );
        assert.equal(
          (await context.cookies()).find((cookie) => cookie.name === 'loggedIn')
            ?.value,
          '0',
        );
        await page.reload();
        await page
          .getByRole('heading', { name: 'Sign in to your workspace' })
          .waitFor();
        assert.equal(
          await page
            .getByText('Record not found: instant-user', { exact: false })
            .count(),
          0,
        );
      } else {
        await page
          .getByText(`Record not found: ${scenario.record}`, { exact: false })
          .waitFor();
        assert.equal(
          await page.evaluate(
            () => JSON.parse(localStorage.getItem('@AUTH')).token,
          ),
          token,
        );
      }
      console.log(`PASS: ${scenario.name}`);
    } catch (error) {
      if (page) {
        console.error((await page.locator('body').innerText()).slice(0, 2000));
        await page.screenshot({
          path: '/tmp/dashboard-auth-regression-failure.png',
          fullPage: true,
        });
      }
      throw error;
    } finally {
      await context.close();
    }
  }
} finally {
  await browser.close();
}
