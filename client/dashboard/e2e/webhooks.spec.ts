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

test.describe('webhooks', () => {
  test.beforeEach(async ({ page }) => {
    await signIn(page);
  });

  test('lists webhooks and opens the create sheet @smoke', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/apps/app-1/webhooks');

    await expect(
      page.getByRole('heading', { level: 1, name: 'Webhooks' }),
    ).toBeVisible();
    const table = page.getByRole('table', { name: 'Webhooks' });
    await expect(table).toContainText('https://example.com/hooks/instant');
    await expect(table).toContainText('Delivered');
    await checkA11y();

    await page.getByRole('button', { name: 'Create webhook' }).click();
    const sheet = page.getByRole('dialog');
    await expect(
      sheet.getByRole('heading', { name: 'Create webhook' }),
    ).toBeVisible();
    await expect(sheet.getByLabel('URL')).toBeVisible();
    await checkA11y();
  });

  test('shows the signing card and a delivery payload', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/apps/app-1/webhooks/webhook-1');

    await expect(
      page.getByRole('heading', {
        level: 1,
        name: 'https://example.com/hooks/instant',
      }),
    ).toBeVisible();
    await expect(page.getByText(/jwks\.json$/)).toBeVisible();
    const deliveries = page.getByRole('table', { name: 'Webhook deliveries' });
    await expect(deliveries).toContainText('HTTP 503');
    await checkA11y();

    await page.getByRole('button', { name: '0/328/48953748' }).click();
    const sheet = page.getByRole('dialog');
    await expect(
      sheet.getByRole('heading', { name: 'Delivery' }),
    ).toBeVisible();
    await expect(sheet.getByText(/Buy milk/)).toBeVisible();
    await checkA11y();
  });
});
