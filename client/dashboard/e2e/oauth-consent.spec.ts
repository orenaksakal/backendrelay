import type { Page } from '@playwright/test';
import {
  mockConsent,
  mockConsentRedirectId,
} from '../src/test/fixtures/api/identity';
import { expect, test } from './fixtures';

const SESSION = JSON.stringify({
  token: 'mock-session-token',
  user: { id: 'user-1', email: 'owner@example.com' },
});

const CONSENT_PATH = `/platform/oauth/start?redirect-id=${mockConsentRedirectId}`;

async function seedSession(page: Page): Promise<void> {
  await page.addInitScript((session) => {
    window.localStorage.setItem('br.session', session);
  }, SESSION);
}

test.describe('OAuth consent', () => {
  test('shows the request and reaches both decisions by keyboard @smoke', async ({
    page,
    checkA11y,
  }) => {
    await seedSession(page);
    await page.goto(CONSENT_PATH);

    await expect(
      page.getByRole('heading', {
        level: 1,
        name: `${mockConsent.appName} wants to access your BackendRelay account`,
      }),
    ).toBeVisible();
    await expect(page.getByText(mockConsent.userEmail)).toBeVisible();
    await expect(page.getByText('https://client.test')).toBeVisible();

    await checkA11y();

    await page.keyboard.press('Tab');
    await expect(
      page.getByRole('link', { name: 'BackendRelay home' }),
    ).toBeFocused();
    await page.keyboard.press('Tab');
    await expect(
      page.getByRole('button', { name: 'Not you? Sign out' }),
    ).toBeFocused();
    await page.keyboard.press('Tab');
    await expect(page.getByRole('button', { name: 'Deny' })).toBeFocused();
    await page.keyboard.press('Tab');
    await expect(page.getByRole('button', { name: 'Allow' })).toBeFocused();
  });

  test('signs a signed-out visitor in first', async ({ page, checkA11y }) => {
    await page.goto(CONSENT_PATH);

    await expect(
      page.getByRole('heading', { level: 1, name: 'Sign in to BackendRelay' }),
    ).toBeVisible();

    await checkA11y();
  });
});
