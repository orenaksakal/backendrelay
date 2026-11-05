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
 * The Storage screen reads `$files` through a live Instant connection, so the
 * mock-mode browser suite covers the role gate, the page shell and
 * accessibility; the folder tree and upload flow are exercised in Vitest and
 * against the real stack in D9.2.
 */
test.describe('storage', () => {
  test.beforeEach(async ({ page }) => {
    await signIn(page);
  });

  test('stops below admin access and explains the role @smoke', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/apps/app-2/storage');

    await expect(
      page.getByRole('heading', { level: 1, name: 'Storage' }),
    ).toBeVisible();
    await expect(
      page.getByText("You don't have access to files"),
    ).toBeVisible();
    await expect(
      page.getByText('The file list needs admin access to this app.'),
    ).toBeVisible();
    await expect(page.getByText('This needs the admin role.')).toBeVisible();
    await expect(page.getByRole('table', { name: 'Files' })).toHaveCount(0);
    await checkA11y();
  });
});

/**
 * DASH.fix10: page content must never paint above an open sheet. The upload
 * progress panel is page content, so when the live journey uploaded a file and
 * then opened another file's sheet, the panel sat over the footer's `Delete`
 * button and swallowed the click. The mocked run keeps the same viewport and
 * animation settings as that journey.
 */
test.describe('storage with an open file sheet', () => {
  test.use({ reducedMotion: 'no-preference' });

  test('clicks the file sheet footer Delete while the upload panel is open', async ({
    page,
  }) => {
    await page.addInitScript((session) => {
      window.localStorage.setItem('br.session', session);
      window.localStorage.setItem('br.mockInstant', '1');
    }, SESSION);
    await page.goto('/apps/app-1/storage');

    await page.getByLabel('Choose files to upload').setInputFiles({
      name: 'second.txt',
      mimeType: 'text/plain',
      buffer: Buffer.from('second file'),
    });
    const uploads = page.getByRole('region', { name: 'Uploads' });
    await expect(uploads.getByText('Uploaded', { exact: true })).toBeVisible();

    await page.getByRole('button', { name: 'notes.txt', exact: true }).click();
    const sheet = page.getByRole('dialog');
    await expect(
      sheet.getByRole('heading', { name: 'notes.txt' }),
    ).toBeVisible();

    // The finished upload panel still occupies the footer band. Only a sheet
    // painted above it can pass Playwright's hit-target check.
    await expect(uploads).toBeVisible();
    await sheet.getByRole('button', { name: 'Delete', exact: true }).click();

    const confirm = page.getByRole('dialog', { name: 'Delete this file?' });
    await confirm.getByRole('button', { name: 'Delete file' }).click();
    await expect(page.getByText('1 file deleted')).toBeVisible();
  });
});
