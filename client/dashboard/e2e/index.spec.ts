import { expect, test } from './fixtures';

test.describe('index route', () => {
  test('sends signed-out visitors to the sign-in screen @smoke', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/');

    await expect(page).toHaveURL(/\/login\?redirect=/);
    await expect(
      page.getByRole('heading', {
        level: 1,
        name: 'Sign in to BackendRelay',
      }),
    ).toBeVisible();

    await checkA11y();
  });
});
