import { readFile } from 'node:fs/promises';
import { expect, test } from '@playwright/test';

/**
 * D9.2 live journey. `self-hosting/run-dashboard-browser-acceptance.sh` builds
 * this dashboard against a disposable `docker-compose.test.yml` stack (Postgres,
 * RustFS and the Rust server) and serves the static build with `vite preview`.
 *
 * The test stack has no email provider, so the server logs the dashboard magic
 * code when delivery is disabled. The script captures the server log and
 * passes its path as `BR_LIVE_SERVER_LOG`.
 */

const PNG = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGMIqFgAAAKEAWkyLad+AAAAAElFTkSuQmCC',
  'base64',
);

function uniqueSuffix(): string {
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;
}

/** The dashboard code is one JSON log event written when delivery is off. */
async function readMagicCode(email: string): Promise<string> {
  const logPath = process.env.BR_LIVE_SERVER_LOG;
  if (logPath === undefined || logPath === '') {
    throw new Error('BR_LIVE_SERVER_LOG must point at the server log file');
  }
  const deadline = Date.now() + 60_000;
  let found: string | undefined;
  while (Date.now() < deadline) {
    let log = '';
    try {
      log = await readFile(logPath, 'utf8');
    } catch {
      // The follower may not have created the file yet.
    }
    for (const line of log.split('\n')) {
      const start = line.indexOf('{');
      if (start === -1) continue;
      let event: unknown;
      try {
        event = JSON.parse(line.slice(start));
      } catch {
        continue;
      }
      if (typeof event !== 'object' || event === null) continue;
      const record = event as Record<string, unknown>;
      if (record['recipient'] !== email) continue;
      const code = record['verification_code'];
      if (typeof code === 'string' && /^\d{6}$/.test(code)) {
        found = code;
      }
    }
    if (found !== undefined) return found;
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
  throw new Error(`no dashboard magic code was logged for ${email}`);
}

test('runs the live dashboard journey', async ({ page }) => {
  // Bound the whole journey so a detached or invisible control fails fast
  // instead of burning the project's 15-minute ceiling.
  test.setTimeout(300_000);

  const suffix = uniqueSuffix();
  const email =
    process.env.BR_LIVE_LOGIN_EMAIL ?? `dashboard-live-${suffix}@example.test`;
  const appTitle = `live-${suffix}`;
  const restoredTitle = `${appTitle}-restored`;
  const entityTitle = `Entity ${suffix}`;
  const editedTitle = `Edited ${suffix}`;
  const fileName = `live-${suffix}.png`;
  const webhookUrl = `https://example.com/backendrelay/${suffix}`;
  const backupDescription = `live-backup-${suffix}`;

  await page.addInitScript(() => {
    // Headless Chromium advertises the File System Access API but cannot show
    // a folder picker, so download "Download all" exercises the fallback.
    delete (window as unknown as { showDirectoryPicker?: unknown })
      .showDirectoryPicker;
  });

  let appId = '';

  await test.step('signs in with a magic code', async () => {
    await page.goto('/login?redirect=%2Fnew');
    await expect(
      page.getByRole('heading', { level: 1, name: 'Sign in to BackendRelay' }),
    ).toBeVisible();

    await page.getByLabel('Email').fill(email);
    await page.getByRole('button', { name: 'Send code' }).click();
    await expect(page.getByText(/We sent a 6-digit code to/)).toBeVisible();

    const code = await readMagicCode(email);
    await page.getByLabel('Code').fill(code);

    await expect(page).toHaveURL(/\/new$/);
    await expect(
      page.getByRole('heading', { level: 1, name: 'Create app' }),
    ).toBeVisible();
  });

  await test.step('creates an app', async () => {
    await page.getByLabel('App name').fill(appTitle);
    await page.getByRole('button', { name: 'Create app' }).click();

    await expect(page).toHaveURL(/\/apps\/[^/]+\/overview$/);
    await expect(
      page.getByRole('heading', { level: 1, name: appTitle }),
    ).toBeVisible();
    appId = decodeURIComponent(
      /\/apps\/([^/]+)\/overview$/.exec(page.url())?.[1] ?? '',
    );
    expect(appId).not.toBe('');
  });

  await test.step('adds and restores schema attributes', async () => {
    await page.goto(`/apps/${appId}/schema`);
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

    await page.getByRole('button', { name: 'Add attribute' }).click();
    const titleSheet = page.getByRole('dialog');
    await titleSheet.getByLabel('Attribute name').fill('title');
    await titleSheet.getByRole('button', { name: 'Add attribute' }).click();
    await expect(page.getByText('Added title')).toBeVisible();

    const attributes = page.getByRole('table', {
      name: 'Attributes in todos',
    });
    await expect(attributes).toContainText('title');

    await page.getByRole('button', { name: 'Add attribute' }).click();
    const notesSheet = page.getByRole('dialog');
    await notesSheet.getByLabel('Attribute name').fill('notes');
    await notesSheet.getByRole('button', { name: 'Add attribute' }).click();
    await expect(page.getByText('Added notes')).toBeVisible();
    await expect(attributes).toContainText('notes');

    await attributes.getByRole('button', { name: 'Actions for notes' }).click();
    await page.getByRole('menuitem', { name: 'Delete' }).click();
    const deleteDialog = page.getByRole('dialog');
    await deleteDialog.getByRole('textbox').fill('notes');
    await deleteDialog
      .getByRole('button', { name: 'Delete attribute' })
      .click();
    await expect(
      page.getByText('notes deleted; restore it from the Deleted tab'),
    ).toBeVisible();

    await page.getByRole('tab', { name: 'Deleted' }).click();
    const deleted = page.getByRole('table', { name: 'Deleted attributes' });
    const deletedRow = deleted.getByRole('row').filter({ hasText: 'notes' });
    await expect(deletedRow).toBeVisible();
    await deletedRow.getByRole('button', { name: 'Restore' }).click();
    await expect(page.getByText('todos.notes restored')).toBeVisible();

    await page.getByRole('tab', { name: 'Attributes' }).click();
    await expect(attributes).toContainText('notes');
  });

  await test.step('creates, edits and deletes an Explorer entity', async () => {
    await page.goto(`/apps/${appId}/explorer?ns=todos`);
    await expect(
      page.getByRole('heading', { level: 1, name: 'Explorer' }),
    ).toBeVisible();

    await page.getByRole('button', { name: 'New entity' }).click();
    const sheet = page.getByRole('dialog');
    await expect(
      sheet.getByRole('heading', { name: 'todos entity' }),
    ).toBeVisible();
    await sheet.getByLabel('title', { exact: true }).fill(entityTitle);
    await sheet.getByRole('button', { name: 'Save', exact: true }).click();
    await expect(page.getByText('Entity created')).toBeVisible();

    const createdRow = page.locator('.ag-row').filter({ hasText: entityTitle });
    await expect(createdRow).toBeVisible();

    await createdRow.getByText(entityTitle).dblclick();
    const editor = page.locator('.ag-cell-inline-editing input');
    await editor.fill(editedTitle);
    await editor.press('Enter');
    await expect(page.getByText(editedTitle)).toBeVisible();

    const updatedRow = page.locator('.ag-row').filter({ hasText: editedTitle });
    const entityId = await updatedRow.getAttribute('row-id');
    expect(entityId).not.toBeNull();

    await page.goto(
      `/apps/${appId}/explorer?ns=todos&entity=${encodeURIComponent(entityId ?? '')}`,
    );
    const entitySheet = page.getByRole('dialog');
    await expect(
      entitySheet.getByRole('heading', { name: 'todos entity' }),
    ).toBeVisible();
    await entitySheet
      .getByRole('button', { name: 'More save actions' })
      .click();
    await page.getByRole('menuitem', { name: 'Delete entity' }).click();
    const confirmDelete = page.getByRole('dialog').last();
    await confirmDelete.getByRole('button', { name: 'Delete entity' }).click();
    await expect(page.getByText('Entity deleted')).toBeVisible();
    await expect(page.getByText(editedTitle)).toHaveCount(0);
  });

  await test.step('saves a rule and runs test-as', async () => {
    await page.goto(`/apps/${appId}/permissions`);
    await expect(
      page.getByRole('heading', { level: 1, name: 'Permissions' }),
    ).toBeVisible();

    await page
      .getByRole('button', { name: 'Lock update rule (admin only)' })
      .click();
    await expect(page.getByText('You have unsaved changes')).toBeVisible();

    await page.getByRole('button', { name: 'Save', exact: true }).click();
    const review = page.getByRole('dialog');
    await expect(
      review.getByRole('heading', { name: 'Review permission changes' }),
    ).toBeVisible();
    await review.getByRole('button', { name: 'Save rules' }).click();
    await expect(page.getByText('Permissions saved')).toBeVisible();

    await page.getByRole('button', { name: 'Test as…' }).click();
    const testAs = page.getByRole('dialog');
    await expect(
      testAs.getByRole('heading', { name: 'Test as…' }),
    ).toBeVisible();
    await testAs.getByRole('button', { name: 'Run', exact: true }).click();
    await expect(testAs.getByText(/permission check/)).toBeVisible();
    await expect(
      testAs.getByRole('table', { name: 'Permission checks' }),
    ).toBeVisible();
    await testAs.getByRole('button', { name: 'Close' }).first().click();
  });

  await test.step('uploads, previews and deletes a file', async () => {
    await page.goto(`/apps/${appId}/storage`);
    await expect(
      page.getByRole('heading', { level: 1, name: 'Storage' }),
    ).toBeVisible();

    await page.getByLabel('Choose files to upload').setInputFiles({
      name: fileName,
      mimeType: 'image/png',
      buffer: PNG,
    });
    await expect(page.getByText('1 file uploaded')).toBeVisible({
      timeout: 120_000,
    });

    const fileButton = page.getByRole('button', {
      name: fileName,
      exact: true,
    });
    await expect(fileButton).toBeVisible();
    await fileButton.click();

    const fileSheet = page.getByRole('dialog');
    await expect(
      fileSheet.getByRole('heading', { name: fileName }),
    ).toBeVisible();
    const preview = fileSheet.getByRole('img', { name: fileName });
    await expect(preview).toBeVisible();
    await expect
      .poll(
        async () =>
          preview.evaluate(
            (element) =>
              element instanceof HTMLImageElement &&
              element.complete &&
              element.naturalWidth > 0,
          ),
        { timeout: 60_000 },
      )
      .toBe(true);

    await fileSheet
      .getByRole('button', { name: 'Delete', exact: true })
      .click();
    const deleteDialog = page.getByRole('dialog').last();
    await deleteDialog.getByRole('button', { name: 'Delete file' }).click();
    await expect(page.getByText('1 file deleted')).toBeVisible();
    await expect(
      page.getByRole('button', { name: fileName, exact: true }),
    ).toHaveCount(0);
  });

  await test.step('creates and disables a webhook', async () => {
    await page.goto(`/apps/${appId}/webhooks`);
    await expect(
      page.getByRole('heading', { level: 1, name: 'Webhooks' }),
    ).toBeVisible();

    await page.getByRole('button', { name: 'Create webhook' }).first().click();
    const form = page.getByRole('dialog');
    await form.getByLabel('URL').fill(webhookUrl);
    const namespaceInput = form.getByRole('combobox', {
      name: 'Namespaces',
    });
    await namespaceInput.focus();
    // `todos` sorts after the `$` system namespaces, so ArrowUp opens the list
    // on its last option.
    await page.keyboard.press('ArrowUp');
    await page.keyboard.press('Enter');
    await expect(
      form.getByRole('button', { name: 'Remove todos' }),
    ).toBeVisible();
    // A multi-select keeps the list open after choosing; Escape closes only the
    // listbox so the footer becomes reachable again.
    await page.keyboard.press('Escape');
    await form.getByRole('button', { name: 'Create webhook' }).click();
    await expect(page.getByText('Webhook created')).toBeVisible();

    const table = page.getByRole('table', { name: 'Webhooks' });
    const row = table.getByRole('row').filter({ hasText: webhookUrl });
    await expect(row).toBeVisible();

    await row
      .getByRole('button', { name: `Actions for ${webhookUrl}` })
      .click();
    await page.getByRole('menuitem', { name: 'Disable' }).click();
    const disableDialog = page.getByRole('dialog');
    await disableDialog
      .getByRole('button', { name: 'Disable webhook' })
      .click();
    await expect(page.getByText('Webhook disabled')).toBeVisible();
    await expect(row).toContainText('Disabled');
  });

  let restoredAppId = '';

  const backupEntityTitle = `Backup ${suffix}`;

  await test.step('creates and downloads a backup', async () => {
    // Keep one entity in the app so the snapshot has data to list and download.
    await page.goto(`/apps/${appId}/explorer?ns=todos`);
    await page.getByRole('button', { name: 'New entity' }).click();
    const entitySheet = page.getByRole('dialog');
    await entitySheet
      .getByLabel('title', { exact: true })
      .fill(backupEntityTitle);
    await entitySheet
      .getByRole('button', { name: 'Save', exact: true })
      .click();
    await expect(page.getByText('Entity created')).toBeVisible();

    await page.goto(`/apps/${appId}/backups`);
    await expect(
      page.getByRole('heading', { level: 1, name: 'Backups' }),
    ).toBeVisible();

    await page.getByRole('button', { name: 'Create backup' }).first().click();
    const createDialog = page.getByRole('dialog');
    await createDialog
      .getByLabel('Description (optional)')
      .fill(backupDescription);
    await createDialog.getByRole('button', { name: 'Create backup' }).click();
    await expect(page.getByText('Backup started')).toBeVisible();

    const table = page.getByRole('table', { name: 'Backups' });
    const row = table.getByRole('row').filter({ hasText: backupDescription });
    await expect(row).toBeVisible({ timeout: 240_000 });

    await row
      .getByRole('button', { name: `Actions for ${backupDescription}` })
      .click();
    await page.getByRole('menuitem', { name: 'Download' }).click();
    const downloadSheet = page.getByRole('dialog');
    await expect(
      downloadSheet.getByRole('heading', { name: 'Download backup' }),
    ).toBeVisible();
    const downloadButton = downloadSheet.getByRole('button', {
      name: 'Download all',
    });
    await expect(downloadButton).toBeEnabled({ timeout: 120_000 });

    const [download] = await Promise.all([
      page.waitForEvent('download', { timeout: 120_000 }),
      downloadButton.click(),
    ]);
    expect(download.suggestedFilename()).not.toBe('');
    await downloadSheet.getByRole('button', { name: 'Close' }).first().click();
  });

  await test.step('restores the backup into a new app', async () => {
    const table = page.getByRole('table', { name: 'Backups' });
    const row = table.getByRole('row').filter({ hasText: backupDescription });
    await row
      .getByRole('button', { name: `Actions for ${backupDescription}` })
      .click();
    await page.getByRole('menuitem', { name: 'Restore' }).click();

    const restoreSheet = page.getByRole('dialog');
    await expect(
      restoreSheet.getByRole('heading', { name: 'Restore backup' }),
    ).toBeVisible();
    await restoreSheet.getByLabel('New app title').fill(restoredTitle);
    await restoreSheet.getByRole('button', { name: 'Continue' }).click();
    await restoreSheet.getByRole('button', { name: 'Continue' }).click();
    await restoreSheet.getByRole('textbox').fill('restore');
    await restoreSheet.getByRole('button', { name: 'Restore backup' }).click();
    await expect(page.getByText('Restore started')).toBeVisible();

    const openRestored = restoreSheet.getByRole('link', {
      name: `Open ${restoredTitle}`,
    });
    await expect(openRestored).toBeVisible();
    const href = await openRestored.getAttribute('href');
    restoredAppId = /\/apps\/([^/]+)\//.exec(href ?? '')?.[1] ?? '';
    expect(restoredAppId).not.toBe('');

    await expect(restoreSheet.getByText('Done', { exact: true })).toBeVisible({
      timeout: 240_000,
    });
    await restoreSheet.getByRole('button', { name: 'Close' }).first().click();

    await page.goto(`/apps/${restoredAppId}/overview`);
    await expect(
      page.getByRole('heading', { level: 1, name: restoredTitle }),
    ).toBeVisible();

    await page.goto(`/apps/${restoredAppId}/explorer?ns=todos`);
    await expect(page.getByText(backupEntityTitle)).toBeVisible();
  });

  await test.step('rotates the admin token', async () => {
    await page.goto(`/apps/${appId}/settings?tab=token`);
    await expect(
      page.getByRole('heading', { level: 1, name: 'Settings' }),
    ).toBeVisible();

    await page.getByRole('button', { name: 'Rotate' }).click();
    const rotateDialog = page.getByRole('dialog');
    await rotateDialog.getByRole('textbox').fill('rotate');
    await rotateDialog.getByRole('button', { name: 'Rotate' }).click();

    const newTokenDialog = page.getByRole('dialog');
    await expect(
      newTokenDialog.getByRole('heading', {
        name: 'Copy the new admin token',
      }),
    ).toBeVisible();
    await expect(
      newTokenDialog.getByRole('button', { name: 'Copy admin token' }),
    ).toBeVisible();
    await newTokenDialog.getByRole('button', { name: "I've saved it" }).click();
    await expect(newTokenDialog).toHaveCount(0);
  });

  await test.step('deletes the app', async () => {
    await page.goto(`/apps/${appId}/settings?tab=danger`);
    await page.getByRole('button', { name: 'Delete app' }).click();
    const dialog = page.getByRole('dialog');
    await expect(
      dialog.getByRole('heading', { name: `Delete ${appTitle}?` }),
    ).toBeVisible();
    await dialog.getByRole('textbox').fill(appTitle);
    await dialog.getByRole('button', { name: 'Delete app' }).click();

    await expect(page.getByText('App scheduled for deletion')).toBeVisible();
    await expect(page).toHaveURL(/\/w\/[^/]+(?:\?.*)?$/);
  });
});
