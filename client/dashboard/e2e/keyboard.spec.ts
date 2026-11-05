import type { Locator, Page } from '@playwright/test';
import { expect, test } from './fixtures';

/**
 * D9.1 keyboard-only pass over the primary operator flows. Each flow is driven
 * with focus and key presses; the mouse is never used. The per-screen specs
 * cover the same screens in depth, so this spec is the cross-cutting guarantee
 * that the whole journey stays reachable without a pointer.
 */

const SESSION = JSON.stringify({
  token: 'e2e-session-token',
  user: { id: 'user-1', email: 'owner@example.com' },
});

async function seedSession(page: Page): Promise<void> {
  await page.addInitScript((session) => {
    window.localStorage.setItem('br.session', session);
  }, SESSION);
}

/**
 * Entity writes need a live Instant socket. In mock builds the data client is
 * replaced by `src/test/stubs/instant-react.ts` only when this flag is set.
 */
async function seedEntitySession(page: Page): Promise<void> {
  await page.addInitScript((session) => {
    window.localStorage.setItem('br.session', session);
    window.localStorage.setItem('br.mockInstant', '1');
  }, SESSION);
}

async function focusAndPress(
  page: Page,
  locator: Locator,
  key = 'Enter',
): Promise<void> {
  await locator.focus();
  await page.keyboard.press(key);
}

test.describe('keyboard-only flows', () => {
  test('signs in with a code', async ({ page, checkA11y }) => {
    await page.goto('/login?redirect=%2Fapps%2Fapp-1%2Foverview');

    await expect(page.getByLabel('Email')).toBeFocused();
    await page.keyboard.type('owner@example.com');
    await page.keyboard.press('Tab');
    await expect(page.getByRole('button', { name: 'Send code' })).toBeFocused();
    await page.keyboard.press('Enter');

    await expect(page.getByLabel('Code')).toBeFocused();
    await page.keyboard.type('123456');

    await expect(page).toHaveURL(/\/apps\/app-1\/overview$/);
    await expect(
      page.getByRole('heading', { level: 1, name: 'todo-app' }),
    ).toBeVisible();
    await checkA11y();
  });

  test('creates an app', async ({ page, checkA11y }) => {
    await seedSession(page);
    await page.goto('/new');

    await expect(page.getByLabel('App name')).toBeFocused();
    await page.keyboard.type('keyboard-app');
    await focusAndPress(page, page.getByRole('button', { name: 'Create app' }));

    await expect(page).toHaveURL(/\/apps\/.+\/overview$/);
    await expect(page.getByText('App created')).toBeVisible();
    await checkA11y();
  });

  test('edits and saves a permission rule', async ({ page, checkA11y }) => {
    await seedSession(page);
    await page.goto('/apps/app-1/permissions');

    await expect(
      page.getByRole('heading', { level: 2, name: 'todos' }),
    ).toBeVisible();

    await focusAndPress(
      page,
      page.getByRole('button', { name: 'Lock update rule (admin only)' }),
    );
    await expect(page.getByText('You have unsaved changes')).toBeVisible();

    await focusAndPress(page, page.getByRole('button', { name: 'Save' }));
    const dialog = page.getByRole('dialog');
    await expect(
      dialog.getByRole('heading', { name: 'Review permission changes' }),
    ).toBeVisible();
    await focusAndPress(
      page,
      dialog.getByRole('button', { name: 'Save rules' }),
    );

    await expect(page.getByText('Permissions saved')).toBeVisible();
    await expect(page.getByText('You have unsaved changes')).toBeHidden();
    await checkA11y();
  });

  test('creates a webhook', async ({ page, checkA11y }) => {
    await seedSession(page);
    await page.goto('/apps/app-1/webhooks');

    await expect(
      page.getByRole('heading', { level: 1, name: 'Webhooks' }),
    ).toBeVisible();
    await focusAndPress(
      page,
      page.getByRole('button', { name: 'Create webhook' }),
    );

    const sheet = page.getByRole('dialog');
    await expect(
      sheet.getByRole('heading', { name: 'Create webhook' }),
    ).toBeVisible();

    const url = sheet.getByRole('textbox', { name: 'URL' });
    await url.focus();
    await page.keyboard.type('https://example.com/hooks/keyboard');

    const namespaces = sheet.getByRole('combobox', { name: 'Namespaces' });
    await namespaces.focus();
    // `todos` sorts after the `$` system namespaces, so ArrowUp opens the
    // list on its last option.
    await page.keyboard.press('ArrowUp');
    await page.keyboard.press('Enter');
    await expect(
      sheet.getByRole('button', { name: 'Remove todos' }),
    ).toBeVisible();
    // A multi-select keeps the list open after choosing; Escape closes only the
    // listbox so the footer becomes reachable again.
    await page.keyboard.press('Escape');

    await focusAndPress(
      page,
      sheet.getByRole('button', { name: 'Create webhook' }),
    );
    await expect(page.getByText('Webhook created')).toBeVisible();
    await expect(page.getByRole('dialog')).toHaveCount(0);
    await checkA11y();
  });

  test('creates a backup', async ({ page, checkA11y }) => {
    await seedSession(page);
    await page.goto('/apps/app-1/backups');

    await expect(page.getByRole('table', { name: 'Backups' })).toBeVisible();
    await focusAndPress(
      page,
      page.getByRole('button', { name: 'Create backup' }),
    );

    const sheet = page.getByRole('dialog');
    await expect(
      sheet.getByRole('heading', { name: 'Create backup' }),
    ).toBeVisible();

    const description = sheet.getByLabel('Description (optional)');
    await description.focus();
    await page.keyboard.type('Before migration');
    await focusAndPress(
      page,
      sheet.getByRole('button', { name: 'Create backup' }),
    );

    await expect(page.getByText('Backup started')).toBeVisible();
    await expect(page.getByRole('dialog')).toBeHidden();
    await checkA11y();
  });

  test('invites a member', async ({ page, checkA11y }) => {
    await seedSession(page);
    await page.goto('/w/org-1/members');

    await expect(
      page.getByRole('table', { name: 'Organization members' }),
    ).toBeVisible();
    await focusAndPress(
      page,
      page.getByRole('button', { name: 'Invite member' }),
    );

    const dialog = page.getByRole('dialog');
    await expect(dialog).toBeVisible();
    const email = dialog.getByLabel('Email');
    await email.focus();
    await page.keyboard.type('keyboard@example.com');
    await page.keyboard.press('Enter');

    await expect(dialog).toBeHidden();
    await expect(page.getByText('Invite sent')).toBeVisible();
    await checkA11y();
  });
});

test.describe('keyboard-only flows: entity CRUD', () => {
  test.beforeEach(async ({ page }) => {
    await seedEntitySession(page);
  });

  test('creates an entity through the sheet', async ({ page, checkA11y }) => {
    await page.goto('/apps/app-1/explorer?ns=todos');
    await expect(page.getByText('Buy milk')).toBeVisible({ timeout: 15_000 });

    await focusAndPress(page, page.getByRole('button', { name: 'New entity' }));

    const sheet = page.getByRole('dialog');
    await expect(
      sheet.getByRole('heading', { name: 'todos entity' }),
    ).toBeVisible();
    await checkA11y();
    const title = sheet.getByRole('textbox', { name: 'title' });
    await title.focus();
    await page.keyboard.type('Keyboard todo');
    await focusAndPress(
      page,
      sheet.getByRole('button', { name: 'Save', exact: true }),
    );

    await expect(page.getByText('Entity created')).toBeVisible();
    await expect(page.getByRole('dialog')).toHaveCount(0);
    await expect(page.getByText('Keyboard todo')).toBeVisible();
    await checkA11y();
  });

  test('edits an entity and saves it', async ({ page, checkA11y }) => {
    await page.goto('/apps/app-1/explorer?ns=todos&entity=t1');

    const sheet = page.getByRole('dialog');
    const title = sheet.getByRole('textbox', { name: 'title' });
    await expect(title).toHaveValue('Buy milk');

    await title.focus();
    await page.keyboard.press('ControlOrMeta+A');
    await page.keyboard.type('Buy milk (edited)');
    await expect(title).toHaveValue('Buy milk (edited)');
    await focusAndPress(
      page,
      sheet.getByRole('button', { name: 'Save', exact: true }),
    );

    await expect(page.getByText('Entity saved')).toBeVisible();
    await expect(page.getByText('Buy milk (edited)')).toBeVisible();
    await checkA11y();
  });

  test('deletes an entity from the sheet menu', async ({ page, checkA11y }) => {
    await page.goto('/apps/app-1/explorer?ns=todos&entity=t2');

    const sheet = page.getByRole('dialog');
    await expect(sheet.getByRole('textbox', { name: 'title' })).toHaveValue(
      'Ship v2',
    );

    await focusAndPress(
      page,
      sheet.getByRole('button', { name: 'More save actions' }),
    );
    const deleteItem = page.getByRole('menuitem', { name: 'Delete entity' });
    await expect(deleteItem).toBeVisible();
    await focusAndPress(page, deleteItem);

    const confirm = page.getByRole('dialog', { name: 'Delete this entity?' });
    await expect(confirm).toBeVisible();
    await focusAndPress(
      page,
      confirm.getByRole('button', { name: 'Delete entity' }),
    );

    await expect(page.getByText('Entity deleted')).toBeVisible();
    await expect(page.getByText('Ship v2')).toHaveCount(0);
    await checkA11y();
  });
});
