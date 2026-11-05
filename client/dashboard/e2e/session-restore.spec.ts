import { expect, test } from './fixtures';

/**
 * The session lives in `localStorage`, so only the browser can restore it. A
 * full page load of a deep link must not bounce a signed-in user to `/login`
 * before that restore has happened (DASH.fix4).
 */
test.describe('session restore', () => {
  test('keeps the session across a full page load @smoke', async ({ page }) => {
    await page.goto('/login?redirect=%2Fapps%2Fapp-1%2Foverview');
    await expect(
      page.getByRole('heading', { level: 1, name: 'Sign in to BackendRelay' }),
    ).toBeVisible();

    await page.getByLabel('Email').fill('owner@example.com');
    await page.getByRole('button', { name: 'Send code' }).click();
    await expect(page.getByText(/We sent a 6-digit code to/)).toBeVisible();
    await page.getByLabel('Code').fill('123456');

    await expect(page).toHaveURL(/\/apps\/app-1\/overview$/);
    await expect(
      page.getByRole('heading', { level: 1, name: 'todo-app' }),
    ).toBeVisible();

    // Full document load, not a client-side route change.
    await page.goto('/apps/app-1/schema');

    await expect(page).not.toHaveURL(/\/login/);
    await expect(
      page.getByRole('heading', { level: 1, name: 'Schema' }),
    ).toBeVisible();
  });
});
