import type { Page } from '@playwright/test';
import { expect, test } from './fixtures';

const SESSION = JSON.stringify({
  token: 'mock-session-token',
  user: { id: 'user-1', email: 'owner@example.com' },
});

async function signIn(page: Page): Promise<void> {
  await page.addInitScript((session) => {
    window.localStorage.setItem('br.session', session);
  }, SESSION);
}

test.describe('organization pages', () => {
  test.beforeEach(async ({ page }) => {
    await signIn(page);
  });

  test('manages members and invites by keyboard @smoke', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/w/org-1/members');
    await expect(
      page.getByRole('heading', { level: 1, name: 'Members' }),
    ).toBeVisible();
    const table = page.getByRole('table', { name: 'Organization members' });
    await expect(table).toBeVisible();
    await expect(table.getByText(/Last owner/)).toBeVisible();
    await checkA11y();

    await page.getByRole('button', { name: 'Invite member' }).focus();
    await page.keyboard.press('Enter');

    const inviteDialog = page.getByRole('dialog');
    await expect(inviteDialog).toBeVisible();
    await page.getByLabel('Email').focus();
    await page.keyboard.type('teammate@example.com');
    await page.keyboard.press('Enter');

    await expect(inviteDialog).toBeHidden();
    await expect(page.getByText('Invite sent')).toBeVisible();
    await checkA11y();

    await page.getByRole('tab', { name: /Invites/ }).focus();
    await page.keyboard.press('Enter');
    const invites = page.getByRole('table', { name: 'Pending invites' });
    await expect(invites).toBeVisible();
    await expect(invites.getByText('new@example.com')).toBeVisible();
    await checkA11y();

    const inviteRow = invites.getByRole('row', { name: /new@example.com/ });
    await inviteRow.getByRole('button', { name: 'Revoke' }).focus();
    await page.keyboard.press('Enter');
    await expect(invites.getByText('new@example.com')).toHaveCount(0);

    await page.getByRole('tab', { name: /Members/ }).focus();
    await page.keyboard.press('Enter');
    const memberRow = page
      .getByRole('table', { name: 'Organization members' })
      .getByRole('row', { name: /dev@example.com/ });
    await memberRow.getByRole('button', { name: 'Remove' }).focus();
    await page.keyboard.press('Enter');

    const confirm = page.getByRole('dialog');
    await confirm.getByRole('button', { name: 'Remove member' }).focus();
    await page.keyboard.press('Enter');
    await expect(
      page
        .getByRole('table', { name: 'Organization members' })
        .getByText('dev@example.com'),
    ).toHaveCount(0);
    await checkA11y();
  });

  test('renames the org and shows the delete block', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/w/org-1/settings');
    await expect(
      page.getByRole('heading', { level: 1, name: 'Settings' }),
    ).toBeVisible();
    await checkA11y();

    const name = page.getByLabel('Name');
    await expect(name).toHaveValue('Acme');
    await name.focus();
    await page.keyboard.press('ControlOrMeta+A');
    await page.keyboard.type('Acme Labs');

    await page.getByRole('button', { name: 'Save' }).focus();
    await page.keyboard.press('Enter');
    await expect(page.getByText('Org renamed')).toBeVisible();
    await expect(page.getByLabel('Name')).toHaveValue('Acme Labs');
    await checkA11y();

    await expect(page.getByText('Move or delete its apps first')).toBeVisible();
    await expect(
      page.getByRole('button', { name: 'Delete org' }),
    ).toBeDisabled();
  });
});
