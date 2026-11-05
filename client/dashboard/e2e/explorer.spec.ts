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
 * The Explorer's live grid needs a real Instant connection, so the mock-mode
 * browser suite covers the role gate, the page shell and accessibility; the
 * grid itself is tested in Playwright against the Docker stack (D9.1).
 */
test.describe('explorer', () => {
  test.beforeEach(async ({ page }) => {
    await signIn(page);
  });

  test('stops below admin access and explains the role @smoke', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/apps/app-2/explorer');

    await expect(
      page.getByRole('heading', { level: 1, name: 'Explorer' }),
    ).toBeVisible();
    await expect(
      page.getByText("You don't have access to explorer"),
    ).toBeVisible();
    await expect(
      page.getByText('The explorer needs admin access to this app.'),
    ).toBeVisible();
    await expect(page.getByText('This needs the admin role.')).toBeVisible();
    await expect(
      page.getByRole('complementary', { name: 'Namespaces' }),
    ).toHaveCount(0);
    await checkA11y();

    await page.keyboard.press('Tab');
    await expect(
      page.getByRole('link', { name: 'Skip to content' }),
    ).toBeFocused();
  });
});
