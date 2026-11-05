import type { Page } from '@playwright/test';
import { expect, test } from './fixtures';

const SESSION = JSON.stringify({
  token: 'e2e-session-token',
  user: { id: 'user-1', email: 'owner@example.com' },
});

async function signIn(page: Page): Promise<void> {
  await page.addInitScript((session) => {
    window.localStorage.setItem('br.session', session);
  }, SESSION);
}

test.describe('app overview', () => {
  test.beforeEach(async ({ page }) => {
    await signIn(page);
  });

  test('shows metrics and health, and connects from the keyboard @smoke', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/apps/app-1/overview');

    await expect(
      page.getByRole('heading', { level: 1, name: 'todo-app' }),
    ).toBeVisible();
    await expect(
      page.getByText('Live sessions', { exact: true }),
    ).toBeVisible();
    await expect(page.getByText('87')).toBeVisible();
    await expect(page.getByText('4.2 MB')).toBeVisible();
    await expect(page.getByText('120 MB')).toBeVisible();
    await expect(
      page.getByText('Permissions: 1 namespace allows public writes'),
    ).toBeVisible();
    await expect(page.getByText('Webhooks: 1 disabled')).toBeVisible();

    const origins = page.getByRole('table', { name: 'Live session origins' });
    await expect(origins).toContainText('https://app.example.com');
    await checkA11y();

    const connect = page.getByRole('button', { name: 'Connect' });
    await connect.focus();
    await page.keyboard.press('Enter');

    const sheet = page.getByRole('dialog');
    await expect(
      sheet.getByRole('heading', { name: 'Connect to todo-app' }),
    ).toBeVisible();
    await expect(
      sheet.getByRole('button', { name: 'Copy app ID' }),
    ).toBeVisible();
    await checkA11y();

    await sheet.getByRole('tab', { name: 'CLI' }).focus();
    await page.keyboard.press('Enter');
    await expect(sheet.getByText(/INSTANT_CLI_API_URI=/)).toBeVisible();
    await expect(
      sheet.getByText(/npx @backendrelay\/cli init --app app-1/),
    ).toBeVisible();

    await page.keyboard.press('Escape');
    await expect(sheet).toHaveCount(0);
    await expect(connect).toBeFocused();
  });
});
