import type { Page } from '@playwright/test';
import { expect, test } from './fixtures';
import { mockCreatedPersonalToken } from '../src/test/fixtures/api/account';

const SESSION = JSON.stringify({
  token: 'mock-session-token',
  user: { id: 'user-1', email: 'owner@example.com' },
});

async function signIn(page: Page): Promise<void> {
  await page.addInitScript((session) => {
    window.localStorage.setItem('br.session', session);
  }, SESSION);
}

test.describe('account pages', () => {
  test.beforeEach(async ({ page }) => {
    await signIn(page);
  });

  test('creates and deletes a personal access token @smoke', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/account/tokens');
    await expect(
      page.getByRole('heading', { level: 1, name: 'Tokens' }),
    ).toBeVisible();
    await expect(page.getByText('Platform Token')).toBeVisible();
    await checkA11y();

    await page.getByRole('button', { name: 'Create token' }).focus();
    await page.keyboard.press('Enter');

    const createDialog = page.getByRole('dialog');
    await expect(createDialog).toBeVisible();
    await page.getByLabel('Name').focus();
    await page.keyboard.type('CI deploy');
    await page.keyboard.press('Enter');

    await expect(
      page.getByRole('heading', { name: 'Copy your token' }),
    ).toBeVisible();
    await expect(page.getByText(mockCreatedPersonalToken.token)).toBeVisible();
    await checkA11y();

    await page.getByRole('button', { name: "I've saved it" }).focus();
    await page.keyboard.press('Enter');
    await expect(page.getByText('CI deploy')).toBeVisible();

    const row = page.getByRole('row', { name: /CI deploy/ });
    await row.getByRole('button', { name: 'Delete' }).focus();
    await page.keyboard.press('Enter');

    const confirm = page.getByRole('dialog');
    await confirm.getByRole('button', { name: 'Delete token' }).focus();
    await page.keyboard.press('Enter');

    await expect(page.getByText('CI deploy')).toHaveCount(0);
    await checkA11y();
  });

  test('remembers the default page size and signs out', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/account/profile');
    await expect(
      page.getByRole('heading', { level: 1, name: 'Profile' }),
    ).toBeVisible();
    await checkA11y();

    await expect(
      page.getByRole('switch', {
        name: 'Require unlock before editing data',
      }),
    ).toHaveCount(0);
    const pageSize = page.getByRole('combobox', { name: 'Default page size' });
    await pageSize.click();
    await page.getByRole('option', { name: '100', exact: true }).click();
    await expect(pageSize).toContainText('100');
    expect(
      await page.evaluate(() => window.localStorage.getItem('br.pageSize')),
    ).toBe('100');

    await page.getByRole('button', { name: 'Sign out' }).focus();
    await page.keyboard.press('Enter');

    await expect(page).toHaveURL(/\/login$/);
    await expect(
      page.getByRole('heading', { level: 1, name: 'Sign in to BackendRelay' }),
    ).toBeVisible();
    await checkA11y();
  });

  test('revokes an authorized OAuth app', async ({ page, checkA11y }) => {
    await page.goto('/account/oauth-apps');
    await expect(
      page.getByRole('heading', { level: 1, name: 'OAuth apps' }),
    ).toBeVisible();
    await expect(page.getByText('Linear sync')).toBeVisible();
    await checkA11y();

    const row = page.getByRole('row', { name: /Linear sync/ });
    await row.getByRole('button', { name: 'Revoke access' }).focus();
    await page.keyboard.press('Enter');

    const dialog = page.getByRole('dialog');
    await dialog.getByRole('button', { name: 'Revoke access' }).focus();
    await page.keyboard.press('Enter');

    await expect(page.getByText('Linear sync')).toHaveCount(0);
    await expect(page.getByText('Release notes')).toBeVisible();
    await checkA11y();
  });

  test('accepts an app invite and opens the app', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/account/invites');
    const table = page.getByRole('table', { name: 'Pending invites' });
    await expect(table).toBeVisible();
    await checkA11y();

    const row = table.getByRole('row', { name: /todo-app/ });
    await row.getByRole('button', { name: 'Accept' }).focus();
    await page.keyboard.press('Enter');

    await expect(page).toHaveURL(/\/apps\/app-1\/overview$/);
    await expect(
      page.getByRole('heading', { level: 1, name: 'todo-app' }),
    ).toBeVisible();
    await checkA11y();
  });
});
