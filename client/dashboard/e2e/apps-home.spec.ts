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

test.describe('apps home', () => {
  test.beforeEach(async ({ page }) => {
    await signIn(page);
  });

  test('lists, searches, sorts and pins by keyboard @smoke', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/w/personal');

    await expect(
      page.getByRole('heading', { level: 1, name: 'Apps' }),
    ).toBeVisible();
    const table = page.getByRole('table', { name: 'Apps' });
    await expect(table).toBeVisible();
    await expect(table.getByRole('row', { name: /todo-app/ })).toContainText(
      '4.2 MB',
    );
    await expect(table.getByRole('row', { name: /headshot-ai/ })).toContainText(
      'Read-only',
    );
    await checkA11y();

    // `/` focuses the page search instead of opening the command palette.
    await page.keyboard.press('/');
    const search = page.getByRole('searchbox', { name: 'Search apps' });
    await expect(search).toBeFocused();
    await page.keyboard.type('headshot');
    await expect(table.getByText('headshot-ai')).toBeVisible();
    await expect(table.getByText('todo-app')).toHaveCount(0);

    await page.keyboard.press('Escape');
    await expect(search).toHaveValue('');
    await expect(table.getByText('todo-app')).toBeVisible();

    await table.getByRole('button', { name: 'Sessions' }).focus();
    await page.keyboard.press('Enter');
    await expect(page).toHaveURL(/sort=sessions/);
    await expect(
      table.getByRole('columnheader', { name: 'Sessions' }),
    ).toHaveAttribute('aria-sort', 'ascending');

    await page.getByRole('button', { name: 'Pin headshot-ai' }).focus();
    await page.keyboard.press('Enter');
    await expect(
      page.getByRole('button', { name: 'Unpin headshot-ai' }),
    ).toBeVisible();
    await expect(table.getByRole('row').nth(1)).toContainText('headshot-ai');
    expect(
      await page.evaluate(() =>
        window.localStorage.getItem('br.pins.personal'),
      ),
    ).toBe(JSON.stringify(['app-2']));
    await checkA11y();

    await page.getByRole('button', { name: 'Grid view' }).focus();
    await page.keyboard.press('Enter');
    await expect(table).toHaveCount(0);
    await expect(page.getByRole('link', { name: 'todo-app' })).toBeVisible();
    await expect(
      page.getByRole('button', { name: 'Grid view' }),
    ).toHaveAttribute('aria-pressed', 'true');
    await checkA11y();
  });
});
