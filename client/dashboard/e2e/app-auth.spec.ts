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

test.describe('app auth', () => {
  test.beforeEach(async ({ page }) => {
    await signIn(page);
  });

  test('shows provider cards, clients and the add sheet @smoke', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/apps/app-1/auth');

    await expect(
      page.getByRole('heading', { level: 1, name: 'Auth' }),
    ).toBeVisible();
    await expect(page.getByText('Configured · 2 clients')).toBeVisible();
    await expect(
      page.getByRole('table', { name: 'OAuth clients' }),
    ).toContainText('google-web');
    await checkA11y();

    await page.getByRole('button', { name: 'Configure Google' }).click();
    const sheet = page.getByRole('dialog');
    await expect(
      sheet.getByRole('heading', { name: 'Google client' }),
    ).toBeVisible();
    await expect(sheet.getByText(/\/runtime\/oauth\/callback$/)).toBeVisible();
    await checkA11y();
  });

  test('lists redirect origins and opens the add dialog', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/apps/app-1/auth?tab=origins');

    const table = page.getByRole('table', { name: 'Redirect origins' });
    await expect(table).toContainText('example.com');
    await expect(table).toContainText('todo-preview');
    await checkA11y();

    await page.getByRole('button', { name: 'Add origin' }).click();
    await expect(
      page.getByRole('heading', { name: 'Add origin' }),
    ).toBeVisible();
    await checkA11y();
  });
});
