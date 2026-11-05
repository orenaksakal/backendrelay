import type { Page } from '@playwright/test';
import { expect, test } from './fixtures';

/**
 * DASH.fix8: a popup opened from inside a sheet or dialog must paint above it.
 * Base UI portals menus and selects next to the sheet rather than inside it, so
 * only the z-index tokens keep the options clickable. Every other mocked spec
 * runs with `reducedMotion: 'reduce'`; the live journey runs with animations
 * enabled, so these tests opt in to the same conditions.
 */
const SESSION = JSON.stringify({
  token: 'e2e-session-token',
  user: { id: 'user-1', email: 'owner@example.com' },
});

async function seedMockSession(page: Page): Promise<void> {
  await page.addInitScript((session) => {
    window.localStorage.setItem('br.session', session);
    window.localStorage.setItem('br.mockInstant', '1');
  }, SESSION);
}

test.describe('popups inside a sheet', () => {
  test.use({ reducedMotion: 'no-preference' });

  test('clicks the entity sheet footer menu above the sheet', async ({
    page,
  }) => {
    await seedMockSession(page);
    await page.goto('/apps/app-1/explorer?ns=todos&entity=t2');

    const sheet = page.getByRole('dialog');
    await expect(sheet.getByRole('textbox', { name: 'title' })).toHaveValue(
      'Ship v2',
    );

    const more = sheet.getByRole('button', { name: 'More save actions' });
    await more.click();
    // A menu item under the sheet is still "visible" to Playwright, so the
    // click's hit-target check is what proves the stacking.
    const deleteItem = page.getByRole('menuitem', { name: 'Delete entity' });
    await expect(deleteItem).toBeVisible();
    await deleteItem.click();

    const confirm = page.getByRole('dialog', { name: 'Delete this entity?' });
    await expect(confirm).toBeVisible();
    await confirm.getByRole('button', { name: 'Delete entity' }).click();

    await expect(page.getByText('Entity deleted')).toBeVisible();
    await expect(page.getByText('Ship v2')).toHaveCount(0);
  });

  test('returns focus to the menu trigger after Escape', async ({ page }) => {
    await seedMockSession(page);
    await page.goto('/apps/app-1/explorer?ns=todos&entity=t2');

    const sheet = page.getByRole('dialog');
    const more = sheet.getByRole('button', { name: 'More save actions' });
    await more.click();
    await expect(
      page.getByRole('menuitem', { name: 'Delete entity' }),
    ).toBeVisible();

    await page.keyboard.press('Escape');
    await expect(page.getByRole('menuitem')).toHaveCount(0);
    await expect(more).toBeFocused();
    await expect(sheet).toBeVisible();
  });

  test('clicks an option in the attribute type select above the sheet', async ({
    page,
  }) => {
    await seedMockSession(page);
    await page.goto('/apps/app-1/schema?ns=todos');

    await page.getByRole('button', { name: 'Add attribute' }).click();
    const sheet = page.getByRole('dialog');
    await sheet.getByLabel('Attribute name').fill('due');

    // The Field label names the trigger; Base UI keeps it as "Type".
    const type = sheet.getByRole('combobox', { name: 'Type' });
    await type.click();
    await page.getByRole('option', { name: 'Number' }).click();
    await expect(type).toContainText('number');

    await sheet
      .getByRole('button', { name: 'Add attribute', exact: true })
      .click();
    await expect(page.getByText('Added due')).toBeVisible();
    await expect(
      page.getByRole('table', { name: 'Attributes in todos' }),
    ).toContainText('due');
  });
});
