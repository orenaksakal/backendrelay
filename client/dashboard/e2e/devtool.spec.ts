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
 * C.20 devtool. The Explorer and Playground need a real Instant connection, so
 * the mock-mode suite covers the inline sign-in, the 400×600 frame, the tabs,
 * help links, the close message and accessibility; the data round trips run in
 * the live suite (D9.2).
 */
test.describe('devtool', () => {
  test('signs in inline at 400×600 @smoke', async ({ page, checkA11y }) => {
    await page.setViewportSize({ width: 400, height: 600 });
    await page.goto('/_devtool?appId=app-1');

    await expect(
      page.getByRole('heading', { name: 'Sign in to BackendRelay' }),
    ).toBeVisible();
    await expect(
      page.getByText('BackendRelay devtool', { exact: true }),
    ).toBeVisible();
    await expect(page.getByRole('tab', { name: 'Explorer' })).toHaveCount(0);
    await checkA11y();

    // The embedded card keeps C.1's focus order and does not navigate away.
    await expect(page.getByLabel('Email')).toBeFocused();
    await page.keyboard.type('owner@example.com');
    await page.keyboard.press('Tab');
    await expect(page.getByRole('button', { name: 'Send code' })).toBeFocused();
    await page.keyboard.press('Enter');

    await expect(page.getByText(/We sent a 6-digit code to/)).toBeVisible();
    await expect(page.getByLabel('Code')).toBeFocused();
    await page.keyboard.type('123456');

    await expect(page).toHaveURL(/\/_devtool\?appId=app-1$/);
    await expect(page.getByRole('tab', { name: 'Explorer' })).toBeVisible();
    await expect(page.getByText('todo-app', { exact: true })).toBeVisible();
    await checkA11y();
  });

  test('switches to help and closes the iframe', async ({
    page,
    checkA11y,
  }) => {
    await signIn(page);
    await page.setViewportSize({ width: 400, height: 600 });
    await page.addInitScript(() => {
      Reflect.set(window, '__devtoolClosed', false);
      window.addEventListener('message', (event: MessageEvent) => {
        const data: unknown = event.data;
        if (
          typeof data === 'object' &&
          data !== null &&
          Reflect.get(data, 'type') === 'close'
        ) {
          Reflect.set(window, '__devtoolClosed', true);
        }
      });
    });
    await page.goto('/_devtool?appId=app-1');

    await expect(page.getByText('todo-app', { exact: true })).toBeVisible();
    await expect(page.getByRole('tab', { name: 'Explorer' })).toHaveAttribute(
      'aria-selected',
      'true',
    );
    await checkA11y();

    await page.getByRole('tab', { name: 'Explorer' }).focus();
    await page.keyboard.press('ArrowRight');
    await page.keyboard.press('ArrowRight');
    // Base UI uses manual activation: Enter opens the focused tab.
    await page.keyboard.press('Enter');
    await expect(page.getByRole('tab', { name: 'Help' })).toHaveAttribute(
      'aria-selected',
      'true',
    );
    await expect(
      page.getByRole('link', { name: 'todo-app overview' }),
    ).toHaveAttribute('href', '/apps/app-1/overview');
    await expect(page.getByRole('link', { name: 'Explorer' })).toHaveAttribute(
      'href',
      '/apps/app-1/explorer',
    );
    await checkA11y();

    await page.getByRole('button', { name: 'Close devtool' }).focus();
    await page.keyboard.press('Enter');
    await expect
      .poll(() =>
        page.evaluate(() => Boolean(Reflect.get(window, '__devtoolClosed'))),
      )
      .toBe(true);
  });

  test('explains a missing app id', async ({ page, checkA11y }) => {
    await signIn(page);
    await page.setViewportSize({ width: 400, height: 600 });
    await page.goto('/_devtool');

    await expect(
      page.getByRole('heading', { name: 'No app ID provided' }),
    ).toBeVisible();
    await checkA11y();
  });
});
