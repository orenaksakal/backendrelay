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
 * The Users list reads `$users` through a live Instant connection, so the
 * mock-mode browser suite covers the role gate, the page shell and
 * accessibility; the list itself is exercised in Vitest and against the real
 * stack in D9.2.
 */
test.describe('users', () => {
  test.beforeEach(async ({ page }) => {
    await signIn(page);
  });

  test('stops below admin access and explains the role @smoke', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/apps/app-2/users');

    await expect(
      page.getByRole('heading', { level: 1, name: 'Users' }),
    ).toBeVisible();
    await expect(
      page.getByText("You don't have access to users"),
    ).toBeVisible();
    await expect(
      page.getByText('The user list needs admin access to this app.'),
    ).toBeVisible();
    await expect(page.getByText('This needs the admin role.')).toBeVisible();
    await expect(page.getByRole('table', { name: 'Users' })).toHaveCount(0);
    await checkA11y();
  });
});
