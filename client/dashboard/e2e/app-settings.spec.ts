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

test.describe('app settings', () => {
  test.beforeEach(async ({ page }) => {
    await signIn(page);
  });

  test('edits the name and walks the tabs from the keyboard @smoke', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/apps/app-1/settings');

    await expect(
      page.getByRole('heading', { level: 1, name: 'Settings' }),
    ).toBeVisible();
    const name = page.getByLabel('Name');
    await expect(name).toHaveValue('todo-app');
    await expect(
      page.getByRole('button', { name: 'Copy ID app-1' }),
    ).toBeVisible();
    await checkA11y();

    await name.focus();
    await page.keyboard.press('ControlOrMeta+A');
    await page.keyboard.type('todo-app settings');
    await expect(page.getByText('You have unsaved changes')).toBeVisible();

    await page.getByRole('button', { name: 'Save' }).focus();
    await page.keyboard.press('Enter');
    await expect(page.getByText('App renamed')).toBeVisible();
    await expect(name).toHaveValue('todo-app settings');
    await checkA11y();

    await page.getByRole('tab', { name: 'Admin token' }).focus();
    await page.keyboard.press('Enter');
    await expect(
      page.getByText(
        'Rotating breaks every server that uses the old token until you update it.',
      ),
    ).toBeVisible();
    await expect(
      page.getByRole('button', { name: 'Copy secret' }),
    ).toBeVisible();
    await checkA11y();

    await page.getByRole('tab', { name: 'Team' }).focus();
    await page.keyboard.press('Enter');
    const members = page.getByRole('table', { name: 'App members' });
    await expect(members).toBeVisible();
    await expect(members).toContainText('admin@example.com');
    await expect(
      page.getByRole('table', { name: 'Pending invites' }),
    ).toContainText('new@example.com');
    await checkA11y();

    await page.getByRole('tab', { name: 'Danger zone' }).focus();
    await page.keyboard.press('Enter');
    await expect(
      page.getByRole('button', { name: 'Delete app' }),
    ).toBeVisible();
    await checkA11y();
  });

  test('unlocks a destructive action only for the exact app name', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/apps/app-1/settings?tab=danger');

    const deleteButton = page.getByRole('button', { name: 'Delete app' });
    await deleteButton.focus();
    await page.keyboard.press('Enter');

    const dialog = page.getByRole('dialog');
    await expect(
      dialog.getByRole('heading', { name: 'Delete todo-app?' }),
    ).toBeVisible();
    const confirm = dialog.getByRole('button', { name: 'Delete app' });
    await expect(confirm).toBeDisabled();

    await dialog.getByRole('textbox').focus();
    await page.keyboard.type('todo-app');
    await expect(confirm).toBeEnabled();
    await checkA11y();

    await page.keyboard.press('Escape');
    await expect(dialog).toHaveCount(0);
    await expect(deleteButton).toBeFocused();
  });
});
