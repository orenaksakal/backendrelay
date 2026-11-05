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

/**
 * The JSON view writes through a real dashboard API, so the mock-mode browser
 * suite covers the shell, the draft review dialog and history; the live save
 * round trip runs against the Docker stack (D9.2).
 */
test.describe('permissions', () => {
  test.beforeEach(async ({ page }) => {
    await signIn(page);
  });

  test('edits an action in the visual editor @smoke', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/apps/app-1/permissions');

    await expect(page.getByRole('tab', { name: 'Editor' })).toHaveAttribute(
      'aria-selected',
      'true',
    );
    await expect(
      page.getByRole('heading', { level: 2, name: 'todos' }),
    ).toBeVisible();
    await expect(page.getByLabel('todos update rule')).toContainText(
      'auth.id != null',
    );
    await expect(page.getByText('Everyone').first()).toBeVisible();
    await checkA11y();

    await page
      .getByRole('button', { name: 'Lock update rule (admin only)' })
      .focus();
    await page.keyboard.press('Enter');
    await expect(page.getByText('You have unsaved changes')).toBeVisible();
    await expect(page.getByLabel('todos update rule')).toContainText('false');
  });

  test('opens the JSON view and shows the version history', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/apps/app-1/permissions?view=json');

    await expect(
      page.getByRole('heading', { level: 1, name: 'Permissions' }),
    ).toBeVisible();
    await expect(page.getByRole('tab', { name: 'JSON' })).toHaveAttribute(
      'aria-selected',
      'true',
    );
    await expect(page.getByText('rules.json')).toBeVisible();
    await expect(page.getByText('"todos"').first()).toBeVisible();
    await checkA11y();

    await page.getByRole('tab', { name: 'History' }).click();
    const history = page.getByRole('table', { name: 'Rule versions' });
    await expect(history.getByText('v3')).toBeVisible();
    await expect(history.getByText('v2')).toBeVisible();
  });
});
