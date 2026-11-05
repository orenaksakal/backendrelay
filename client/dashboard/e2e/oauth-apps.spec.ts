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

test.describe('OAuth apps', () => {
  test.beforeEach(async ({ page }) => {
    await signIn(page);
  });

  test('creates an OAuth app from the keyboard @smoke', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/apps/app-1/oauth-apps');

    await expect(
      page.getByRole('heading', { level: 1, name: 'OAuth apps' }),
    ).toBeVisible();
    const table = page.getByRole('table', { name: 'OAuth apps' });
    await expect(table).toContainText('Acme Tools');
    await expect(table).toContainText('Docs portal');
    await checkA11y();

    await page.getByRole('button', { name: 'Create OAuth app' }).focus();
    await page.keyboard.press('Enter');

    const sheet = page.getByRole('dialog');
    await expect(
      sheet.getByRole('heading', { name: 'Create OAuth app' }),
    ).toBeVisible();
    await sheet.getByLabel('Name').focus();
    await page.keyboard.type('Docs bot');
    await sheet.getByRole('button', { name: 'Create OAuth app' }).focus();
    await page.keyboard.press('Enter');

    await expect(page).toHaveURL(
      /\/apps\/app-1\/oauth-apps\/oauth-app-created$/,
    );
    await expect(
      page.getByRole('heading', { level: 1, name: 'Docs bot' }),
    ).toBeVisible();
    await expect(page.getByText(/This app is in test mode/)).toBeVisible();
    await checkA11y();
  });

  test('adds a client secret and shows it once', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/apps/app-1/oauth-apps/oauth-app-1');
    await expect(
      page.getByRole('heading', { level: 1, name: 'Acme Tools' }),
    ).toBeVisible();

    await page.getByRole('button', { name: 'Acme CLI' }).focus();
    await page.keyboard.press('Enter');

    const sheet = page.getByRole('dialog');
    await expect(
      sheet.getByRole('textbox', { name: 'Redirect URL 1' }),
    ).toHaveValue('https://acme.test/callback');
    await checkA11y();

    await sheet.getByRole('button', { name: 'Create secret' }).focus();
    await page.keyboard.press('Enter');

    await expect(
      page.getByRole('heading', { name: 'Copy the client secret' }),
    ).toBeVisible();
    await expect(
      page.getByText('oauth-client-secret-value-0123456789'),
    ).toBeVisible();
    await checkA11y();

    await page.getByRole('button', { name: "I've saved it" }).focus();
    await page.keyboard.press('Enter');
    await expect(page.getByText('9z8y••••••••')).toBeVisible();
  });

  test('saves the app info from the unsaved-changes bar', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/apps/app-1/oauth-apps/oauth-app-1');
    await expect(
      page.getByRole('heading', { level: 1, name: 'Acme Tools' }),
    ).toBeVisible();
    await checkA11y();

    const name = page.getByLabel('Name');
    await name.focus();
    await page.keyboard.press('ControlOrMeta+A');
    await page.keyboard.type('Acme Tools v2');
    await expect(page.getByText('You have unsaved changes')).toBeVisible();

    await page.getByRole('button', { name: 'Save' }).focus();
    await page.keyboard.press('Enter');
    await expect(page.getByText('OAuth app saved')).toBeVisible();
    await expect(name).toHaveValue('Acme Tools v2');
    await checkA11y();

    await page
      .getByRole('main')
      .getByRole('link', { name: 'OAuth apps' })
      .focus();
    await page.keyboard.press('Enter');
    await expect(page).toHaveURL(/\/apps\/app-1\/oauth-apps$/);
  });
});
