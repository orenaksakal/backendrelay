import type { Page } from '@playwright/test';
import { expect, test } from './fixtures';

const SESSION = JSON.stringify({
  token: 'e2e-session-token',
  user: { id: 'user-1', email: 'owner@example.com' },
});

async function signIn(
  page: Page,
  options: { mockInstant?: boolean; emptyStore?: boolean } = {},
): Promise<void> {
  await page.addInitScript(
    ({ session, mockInstant, emptyStore }) => {
      window.localStorage.setItem('br.session', session);
      if (mockInstant) window.localStorage.setItem('br.mockInstant', '1');
      if (emptyStore) window.localStorage.setItem('br.mockInstantEmpty', '1');
    },
    {
      session: SESSION,
      mockInstant: options.mockInstant === true,
      emptyStore: options.emptyStore === true,
    },
  );
}

/**
 * The Schema screen's tables write through a real Instant connection, so the
 * mock-mode browser suite covers the role gate, the shell and accessibility;
 * the add/edit/delete round trip runs against the Docker stack (D9.2).
 */
test.describe('schema', () => {
  test.beforeEach(async ({ page }) => {
    await signIn(page);
  });

  test('stops below admin access and explains the role @smoke', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/apps/app-2/schema');

    await expect(
      page.getByRole('heading', { level: 1, name: 'Schema' }),
    ).toBeVisible();
    await expect(
      page.getByText("You don't have access to schema"),
    ).toBeVisible();
    await expect(
      page.getByText('The schema needs admin access to this app.'),
    ).toBeVisible();
    await expect(page.getByText('This needs the admin role.')).toBeVisible();
    await expect(
      page.getByRole('complementary', { name: 'Namespaces' }),
    ).toHaveCount(0);
    await checkA11y();

    await page.keyboard.press('Tab');
    await expect(
      page.getByRole('link', { name: 'Skip to content' }),
    ).toBeFocused();
  });

  test('creates the first namespace in an empty app', async ({
    page,
    checkA11y,
  }) => {
    await signIn(page, { mockInstant: true, emptyStore: true });
    await page.goto('/apps/app-1/schema');

    await expect(
      page.getByRole('heading', { level: 1, name: 'Schema' }),
    ).toBeVisible();
    const newNamespace = page.getByRole('button', { name: 'New namespace' });
    await expect(newNamespace).toBeVisible();
    await expect(newNamespace).toBeEnabled();
    await checkA11y();

    await newNamespace.click();
    const dialog = page.getByRole('dialog');
    await dialog.getByLabel('Namespace name').fill('todos');
    await dialog.getByRole('button', { name: 'Create namespace' }).click();

    await expect(page.getByText('Namespace todos created')).toBeVisible();
    const rail = page.getByRole('complementary', { name: 'Namespaces' });
    await expect(rail.getByRole('button', { name: /^todos/ })).toBeVisible();
    await expect(
      page.getByRole('table', { name: 'Attributes in todos' }),
    ).toContainText('id');
  });
});

/**
 * DASH.fix6: a popup that is closing must not take pointers from the sheet that
 * opens next. Every other mocked spec runs with `reducedMotion: 'reduce'`, which
 * ends CSS exit transitions immediately and hid this; the live journey runs
 * with animations enabled.
 */
test.describe('schema with animations enabled', () => {
  test.use({ reducedMotion: 'no-preference' });

  test('opens the attribute sheet straight after the namespace toast', async ({
    page,
  }) => {
    await signIn(page, { mockInstant: true, emptyStore: true });
    await page.goto('/apps/app-1/schema');

    await expect(
      page.getByRole('heading', { level: 1, name: 'Schema' }),
    ).toBeVisible();

    await page.getByRole('button', { name: 'New namespace' }).click();
    const namespaceDialog = page.getByRole('dialog');
    await namespaceDialog.getByLabel('Namespace name').fill('todos');
    await namespaceDialog
      .getByRole('button', { name: 'Create namespace' })
      .click();
    await expect(page.getByText('Namespace todos created')).toBeVisible();

    // The toast is still on screen (and the namespace dialog is still exiting);
    // both must release pointer events before the sheet's footer is clicked.
    await page.getByRole('button', { name: 'Add attribute' }).click();
    const attributeSheet = page.getByRole('dialog');
    await attributeSheet.getByLabel('Attribute name').fill('title');
    await attributeSheet.getByRole('button', { name: 'Add attribute' }).click();
    await expect(page.getByText('Added title')).toBeVisible();
    await expect(
      page.getByRole('table', { name: 'Attributes in todos' }),
    ).toContainText('title');
  });

  test('clicks a sheet footer action while the success toast is open', async ({
    page,
  }) => {
    await signIn(page, { mockInstant: true, emptyStore: true });
    await page.goto('/apps/app-1/schema');

    await expect(
      page.getByRole('heading', { level: 1, name: 'Schema' }),
    ).toBeVisible();

    await page.getByRole('button', { name: 'New namespace' }).click();
    const namespaceDialog = page.getByRole('dialog');
    await namespaceDialog.getByLabel('Namespace name').fill('todos');
    await namespaceDialog
      .getByRole('button', { name: 'Create namespace' })
      .click();
    const toast = page.getByText('Namespace todos created');
    await expect(toast).toBeVisible();

    // The sheet's footer sits where the toast used to be; the toast must not
    // intercept the button even while its 4s timer is paused by hover.
    await page.getByRole('button', { name: 'Add attribute' }).click();
    const attributeSheet = page.getByRole('dialog');
    await attributeSheet.getByLabel('Attribute name').fill('title');
    await expect(toast).toBeVisible();
    await attributeSheet.getByRole('button', { name: 'Add attribute' }).click();
    // A click that only lands after the toast times out is too late: the live
    // journey hover pauses the timer and waits forever (DASH.fix9).
    await expect(toast).toBeVisible();
    await expect(page.getByText('Added title')).toBeVisible();
  });
});
