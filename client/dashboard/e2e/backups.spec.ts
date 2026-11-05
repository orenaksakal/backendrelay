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

test.describe('backups', () => {
  test.beforeEach(async ({ page }) => {
    await signIn(page);
  });

  test('creates a backup from the keyboard @smoke', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/apps/app-1/backups');

    await expect(
      page.getByRole('heading', { level: 1, name: 'Backups' }),
    ).toBeVisible();
    const table = page.getByRole('table', { name: 'Backups' });
    await expect(table).toContainText('Nightly');
    await expect(table).toContainText('Before release');
    await checkA11y();

    await page.getByRole('button', { name: 'Create backup' }).focus();
    await page.keyboard.press('Enter');
    await expect(
      page.getByRole('heading', { name: 'Create backup' }),
    ).toBeVisible();
    await checkA11y();

    await page.getByLabel('Description (optional)').focus();
    await page.keyboard.type('Before migration');
    await page
      .getByRole('dialog')
      .getByRole('button', { name: 'Create backup' })
      .focus();
    await page.keyboard.press('Enter');

    await expect(page.getByText('Backup started')).toBeVisible();
    await expect(page.getByRole('dialog')).toBeHidden();
    await checkA11y();
  });

  test('edits the schedule and saves it', async ({ page, checkA11y }) => {
    await page.goto('/apps/app-1/backups?tab=schedule');

    await expect(
      page.getByRole('heading', { level: 1, name: 'Backups' }),
    ).toBeVisible();
    await expect(
      page.getByRole('switch', { name: 'Automatic backups' }),
    ).toBeVisible();
    await checkA11y();

    await page.getByRole('switch', { name: 'Automatic backups' }).focus();
    await page.keyboard.press('Space');
    await expect(page.getByText('You have unsaved changes')).toBeVisible();

    await page.getByRole('button', { name: 'Save' }).focus();
    await page.keyboard.press('Enter');
    await expect(page.getByText('Backup schedule saved')).toBeVisible();
    await expect(page.getByText('You have unsaved changes')).toBeHidden();
    await checkA11y();
  });

  test('shows the backup job history', async ({ page, checkA11y }) => {
    await page.goto('/apps/app-1/backups?tab=jobs');

    await expect(
      page.getByRole('heading', { level: 1, name: 'Backups' }),
    ).toBeVisible();
    const table = page.getByRole('table', { name: 'Backup jobs' });
    await expect(table).toContainText('todo-app (restored 2026-10-20)');
    await expect(table).toContainText('Done');
    await checkA11y();
  });
});
