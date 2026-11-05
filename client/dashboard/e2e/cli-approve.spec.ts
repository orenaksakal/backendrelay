import type { Page } from '@playwright/test';
import { mockCliTicket } from '../src/test/fixtures/api/identity';
import { expect, test } from './fixtures';

const SESSION = JSON.stringify({
  token: 'mock-session-token',
  user: { id: 'user-1', email: 'owner@example.com' },
});

async function seedSession(page: Page): Promise<void> {
  await page.addInitScript((session) => {
    window.localStorage.setItem('br.session', session);
  }, SESSION);
}

test.describe('CLI approval', () => {
  test('approves the ticket with the keyboard @smoke', async ({
    page,
    checkA11y,
  }) => {
    await seedSession(page);
    await page.goto(`/cli/approve?ticket=${mockCliTicket}`);

    await expect(
      page.getByRole('heading', {
        level: 1,
        name: 'Sign in to the BackendRelay CLI?',
      }),
    ).toBeVisible();
    await expect(page.getByText('owner@example.com')).toBeVisible();

    await checkA11y();

    await page.keyboard.press('Tab');
    await expect(
      page.getByRole('link', { name: 'BackendRelay home' }),
    ).toBeFocused();
    await page.keyboard.press('Tab');
    await expect(page.getByRole('button', { name: 'Deny' })).toBeFocused();
    await page.keyboard.press('Tab');
    await expect(page.getByRole('button', { name: 'Approve' })).toBeFocused();
    await page.keyboard.press('Enter');

    await expect(
      page.getByRole('heading', { level: 1, name: 'CLI sign-in approved' }),
    ).toBeVisible();
    await expect(
      page.getByText('You can close this tab and return to your terminal.'),
    ).toBeVisible();

    await checkA11y();
  });
});
