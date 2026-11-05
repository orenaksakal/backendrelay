import { expect, test } from './fixtures';

test.describe('sign-in', () => {
  test('signs in with an emailed code using the keyboard @smoke', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/login?redirect=%2Fapps%2Fapp-1%2Foverview');

    await expect(
      page.getByRole('heading', { level: 1, name: 'Sign in to BackendRelay' }),
    ).toBeVisible();
    // The public config fixture has Google sign-in off (C.1 acceptance).
    await expect(
      page.getByRole('link', { name: /Continue with Google/ }),
    ).toHaveCount(0);

    await checkA11y();

    // Focus starts in the email field, so typing starts there.
    await expect(page.getByLabel('Email')).toBeFocused();
    await page.keyboard.type('owner@example.com');
    await page.keyboard.press('Tab');
    await expect(page.getByRole('button', { name: 'Send code' })).toBeFocused();
    await page.keyboard.press('Enter');

    await expect(page.getByText(/We sent a 6-digit code to/)).toBeVisible();
    await expect(page.getByLabel('Code')).toBeFocused();
    await page.keyboard.type('123456');

    await expect(page).toHaveURL(/\/apps\/app-1\/overview$/);
    await expect(
      page.getByRole('heading', { level: 1, name: 'todo-app' }),
    ).toBeVisible();

    await checkA11y();
  });
});
