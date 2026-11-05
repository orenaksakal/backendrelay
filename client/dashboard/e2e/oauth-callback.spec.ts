import { mockOAuthCode } from '../src/test/fixtures/api/identity';
import { expect, test } from './fixtures';

test.describe('Google callback', () => {
  test('exchanges the code and opens the redirect path @smoke', async ({
    page,
    checkA11y,
  }) => {
    await page.goto(`/dash/oauth/callback?code=${mockOAuthCode}`);

    await expect(page).toHaveURL(/\/apps\/app-1\/overview$/);
    await expect(
      page.getByRole('heading', { level: 1, name: 'todo-app' }),
    ).toBeVisible();

    await checkA11y();
  });

  test('shows the provider error with a way back', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/dash/oauth/callback?error=access_denied');

    await expect(
      page.getByRole('heading', { level: 1, name: 'Google sign-in failed' }),
    ).toBeVisible();
    await expect(page.getByRole('alert')).toContainText('access_denied');

    await checkA11y();

    await page.keyboard.press('Tab');
    await expect(
      page.getByRole('link', { name: 'BackendRelay home' }),
    ).toBeFocused();
    await page.keyboard.press('Tab');
    await expect(
      page.getByRole('link', { name: 'Back to sign in' }),
    ).toBeFocused();
  });
});
