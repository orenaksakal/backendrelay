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

test.describe('operator', () => {
  test.beforeEach(async ({ page }) => {
    await signIn(page);
  });

  test('saves sign-up settings from the keyboard @smoke', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/operator/settings');

    await expect(
      page.getByRole('heading', { level: 1, name: 'Operator settings' }),
    ).toBeVisible();
    await expect(page.getByRole('radio', { name: 'Restricted' })).toBeChecked();
    await expect(page.getByLabel('SSRF allowlist IPs')).toHaveValue(
      '10.0.0.25',
    );
    await checkA11y();

    await page.getByRole('radio', { name: 'Closed' }).focus();
    await page.keyboard.press('Space');
    await expect(page.getByText('You have unsaved changes')).toBeVisible();

    await page.getByRole('button', { name: 'Save' }).focus();
    await page.keyboard.press('Enter');
    await expect(page.getByText('Operator settings saved')).toBeVisible();
    await expect(page.getByText('You have unsaved changes')).toBeHidden();
    await checkA11y();

    await page.getByRole('button', { name: 'Edit team-emails' }).focus();
    await page.keyboard.press('Enter');
    const sheet = page.getByRole('dialog');
    await expect(
      sheet.getByRole('heading', { name: 'Edit team-emails' }),
    ).toBeVisible();
    await checkA11y();

    await sheet.getByRole('button', { name: 'Cancel' }).focus();
    await page.keyboard.press('Enter');
    await expect(page.getByRole('dialog')).toBeHidden();
  });

  test('opens a restore job and cancels it', async ({ page, checkA11y }) => {
    await page.goto('/operator/restores');

    await expect(
      page.getByRole('heading', { level: 1, name: 'Restores' }),
    ).toBeVisible();
    const table = page.getByRole('table', { name: 'Restore jobs' });
    await expect(table).toContainText('headshot-ai copy');
    await expect(table).toContainText('todo-app restore');
    await checkA11y();

    await page
      .getByRole('button', { name: 'View restore job headshot-ai copy' })
      .focus();
    await page.keyboard.press('Enter');
    const detail = page.getByRole('dialog');
    await expect(
      detail.getByRole('heading', { name: 'headshot-ai copy' }),
    ).toBeVisible();
    await expect(detail.getByText('1,200 / 4,800 entities')).toBeVisible();
    await checkA11y();

    await detail.getByRole('button', { name: 'Close' }).focus();
    await page.keyboard.press('Enter');
    await expect(page.getByRole('dialog')).toBeHidden();

    await page
      .getByRole('button', { name: 'Cancel restore job todo-app restore' })
      .focus();
    await page.keyboard.press('Enter');
    const confirm = page.getByRole('dialog');
    await expect(
      confirm.getByText(/Cancel restore of todo-app restore/),
    ).toBeVisible();

    await confirm.getByRole('button', { name: 'Cancel restore job' }).focus();
    await page.keyboard.press('Enter');

    await expect(page.getByText('Restore cancelled')).toBeVisible();
    const row = table.getByRole('row', { name: /todo-app restore/ });
    await expect(row.getByText('Cancelled')).toBeVisible();
    await checkA11y();
  });
});
