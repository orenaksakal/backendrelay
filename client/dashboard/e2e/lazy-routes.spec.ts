import type { Page } from '@playwright/test';
import { expect, test } from './fixtures';

/**
 * D9.3: every app section is a lazy route chunk. TanStack Router splits each
 * route file into its own dynamic import, so visiting a section fetches a new
 * script instead of riding along in the entry bundle.
 *
 * The chunk names come from the route file path (`_authed.apps._appId.<path>`).
 * If the router stops splitting a section, no new matching script arrives and
 * the check fails.
 */

const SESSION = JSON.stringify({
  token: 'e2e-session-token',
  user: { id: 'user-1', email: 'owner@example.com' },
});

const SECTIONS = [
  { label: 'Explorer', path: 'explorer', chunk: 'explorer' },
  { label: 'Schema', path: 'schema', chunk: 'schema' },
  { label: 'Users', path: 'users', chunk: 'users' },
  { label: 'Storage', path: 'storage', chunk: 'storage' },
  { label: 'Permissions', path: 'permissions', chunk: 'permissions' },
  { label: 'Auth', path: 'auth', chunk: 'auth' },
  { label: 'Playground', path: 'playground', chunk: 'playground' },
  { label: 'Webhooks', path: 'webhooks', chunk: 'webhooks_' },
  { label: 'OAuth apps', path: 'oauth-apps', chunk: 'oauth-apps' },
  { label: 'Backups', path: 'backups', chunk: 'backups' },
  { label: 'Settings', path: 'settings', chunk: 'settings' },
] as const;

async function seedSession(page: Page): Promise<void> {
  await page.addInitScript((session) => {
    window.localStorage.setItem('br.session', session);
  }, SESSION);
}

async function loadedScripts(page: Page): Promise<string[]> {
  return page.evaluate(() =>
    performance
      .getEntriesByType('resource')
      .map((entry) => entry.name)
      .filter((name) => name.endsWith('.js')),
  );
}

function routeChunks(scripts: readonly string[], chunk: string): string[] {
  return scripts.filter((name) =>
    name.includes(`_authed.apps._appId.${chunk}-`),
  );
}

test.describe('lazy app routes', () => {
  test('loads every app section as its own chunk', async ({ page }) => {
    await seedSession(page);
    await page.goto('/apps/app-1/overview');
    await expect(
      page.getByRole('heading', { level: 1, name: 'todo-app' }),
    ).toBeVisible();

    const initial = await loadedScripts(page);
    expect(routeChunks(initial, 'overview')).not.toEqual([]);

    const navigation = page
      .locator('aside')
      .getByRole('navigation', { name: 'App' });
    for (const section of SECTIONS) {
      await navigation
        .getByRole('link', { name: section.label, exact: true })
        .click();
      await page.waitForURL(
        (url) => url.pathname === `/apps/app-1/${section.path}`,
      );

      await expect
        .poll(
          async () => {
            const scripts = await loadedScripts(page);
            return routeChunks(scripts, section.chunk).length;
          },
          { timeout: 15_000 },
        )
        .toBeGreaterThan(0);
    }
  });
});
