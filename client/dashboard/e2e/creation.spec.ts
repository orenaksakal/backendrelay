import type { Page } from '@playwright/test';
import { expect, test } from './fixtures';

const SESSION = JSON.stringify({
  token: 'mock-session-token',
  user: { id: 'user-1', email: 'owner@example.com' },
});

async function signIn(page: Page): Promise<void> {
  await page.addInitScript((session) => {
    window.localStorage.setItem('br.session', session);
  }, SESSION);
}

test.describe('app creation', () => {
  test.beforeEach(async ({ page }) => {
    await signIn(page);
  });

  test('creates an app from the keyboard and opens it @smoke', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/new');
    await expect(
      page.getByRole('heading', { level: 1, name: 'Create app' }),
    ).toBeVisible();
    await checkA11y();

    // Focus starts in the app-name field, so typing starts there.
    await expect(page.getByLabel('App name')).toBeFocused();
    await page.keyboard.type('browser-app');
    await page.getByRole('button', { name: 'Create app' }).focus();
    await page.keyboard.press('Enter');

    await expect(page).toHaveURL(/\/apps\/.+\/overview$/);
    await expect(page.getByText('App created')).toBeVisible();
    await checkA11y();
  });

  test('creates an org from the dialog', async ({ page, checkA11y }) => {
    await page.goto('/new');
    await expect(
      page.getByRole('heading', { level: 1, name: 'Create app' }),
    ).toBeVisible();

    await page.getByRole('button', { name: 'Create an org instead' }).click();
    const dialog = page.getByRole('dialog');
    await expect(dialog).toBeVisible();
    await checkA11y();

    await dialog.getByLabel('Org name').fill('Browser org');
    await dialog.getByRole('button', { name: 'Create org' }).click();

    await expect(dialog).toBeHidden();
    await expect(page.getByText('Org created')).toBeVisible();
  });

  test('shows the first-run form at /welcome', async ({ page, checkA11y }) => {
    await page.goto('/welcome');
    await expect(
      page.getByRole('heading', { level: 1, name: 'Create your first app' }),
    ).toBeVisible();
    await expect(page.getByLabel('App name')).toBeVisible();
    await expect(
      page.getByRole('button', { name: 'Create an org instead' }),
    ).toHaveCount(0);
    await checkA11y();
  });
});
