import type { Page } from '@playwright/test';
import { expect, test } from './fixtures';

const session = {
  token: 'e2e-session-token',
  user: { id: 'user-1', email: 'owner@example.com' },
};

async function signIn(page: Page) {
  await page.addInitScript((value: string) => {
    window.localStorage.setItem('br.session', value);
  }, JSON.stringify(session));
}

test.describe('dashboard shell', () => {
  test.beforeEach(async ({ page }) => {
    await signIn(page);
  });

  test('opens the profile and theme menus without crashing @smoke', async ({
    page,
    checkA11y,
  }) => {
    const errors: string[] = [];
    page.on('pageerror', (error) => errors.push(error.message));
    await page.goto('/apps/app-1/overview');

    await page.getByRole('button', { name: 'Account menu' }).click();
    const account = page.getByRole('group', { name: session.user.email });
    await expect(account).toBeVisible();
    await checkA11y();
    await account
      .getByRole('menuitem', { name: 'Account', exact: true })
      .click();

    await expect(page).toHaveURL(/\/account\/profile$/);
    await expect(
      page.getByRole('heading', { level: 1, name: 'Profile' }),
    ).toBeVisible();

    await page.getByRole('button', { name: /^Theme:/ }).click();
    const theme = page.getByRole('group', { name: 'Theme', exact: true });
    await expect(theme).toBeVisible();
    await checkA11y();
    await theme
      .getByRole('menuitemradio', { name: 'Dark', exact: true })
      .click();
    await expect(
      page.getByRole('button', { name: 'Theme: Dark' }),
    ).toBeVisible();
    await expect(page.getByText('Something went wrong!')).toHaveCount(0);
    expect(errors).toEqual([]);
  });

  test('supports keyboard navigation, page titles and collapsing', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/apps/app-1/overview');

    await expect(
      page.getByRole('heading', { level: 1, name: 'todo-app' }),
    ).toBeVisible();
    await expect(page).toHaveTitle('Overview · todo-app · BackendRelay');
    await expect(page.getByRole('navigation', { name: 'App' })).toBeVisible();

    // One `main` landmark, reachable through the first focusable element.
    await expect(page.locator('main#main')).toHaveCount(1);
    await page.keyboard.press('Tab');
    await expect(
      page.getByRole('link', { name: 'Skip to content' }),
    ).toBeFocused();
    await page.keyboard.press('Enter');
    await expect(page.locator('#main')).toBeFocused();

    await checkA11y();

    // Reload so the tab sequence starts at the skip link again.
    await page.goto('/apps/app-1/overview');
    await expect(
      page.getByRole('heading', { level: 1, name: 'todo-app' }),
    ).toBeVisible();

    await page.keyboard.press('Tab');
    await expect(
      page.getByRole('link', { name: 'Skip to content' }),
    ).toBeFocused();

    for (const target of [
      page.getByRole('link', { name: 'BackendRelay home' }),
      page.getByRole('button', { name: /Switch workspace/ }),
      page.getByRole('button', { name: /Switch app/ }),
      page.getByRole('button', { name: 'Search' }),
      page.getByRole('button', { name: /^Theme:/ }),
      page.getByRole('button', { name: 'Account menu' }),
      page.getByRole('link', { name: 'Overview' }),
      page.getByRole('link', { name: 'Explorer' }),
    ]) {
      await page.keyboard.press('Tab');
      await expect(target).toBeFocused();
    }

    await page.getByRole('button', { name: 'Collapse sidebar' }).click();
    await expect(
      page.getByRole('button', { name: 'Expand sidebar' }),
    ).toBeVisible();
    expect(
      await page.evaluate(() => window.localStorage.getItem('br.sidebar')),
    ).toBe('collapsed');

    await page.reload();
    await expect(
      page.getByRole('button', { name: 'Expand sidebar' }),
    ).toBeVisible();
  });

  test('opens and closes the mobile drawer from the keyboard', async ({
    page,
    checkA11y,
  }) => {
    await page.setViewportSize({ width: 800, height: 900 });
    await page.goto('/apps/app-1/overview');

    await expect(page.getByRole('dialog')).toBeHidden();

    await page.getByRole('button', { name: 'Open navigation' }).click();
    const drawer = page.getByRole('dialog');
    await expect(drawer).toBeVisible();
    await expect(drawer.getByRole('navigation', { name: 'App' })).toBeVisible();

    await checkA11y();

    await page.keyboard.press('Escape');
    await expect(drawer).toBeHidden();
  });
});
