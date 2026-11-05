import type { Page } from '@playwright/test';
import { expect, test } from './fixtures';

/**
 * D9.1 reflow checks (WCAG 1.4.10). Browser zoom halves the CSS viewport, so
 * 200% on the 1280px reference screen is 640px and 400% is 320px. At both
 * widths the page must not scroll horizontally: wide tables and grids scroll
 * inside their own container.
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

interface PageCase {
  name: string;
  path: string;
  /** Distinctive content that must survive the reflow. */
  ready: (page: Page) => Promise<void>;
}

const KEY_PAGES: PageCase[] = [
  {
    name: 'apps home',
    path: '/w/personal',
    ready: async (page) => {
      await expect(
        page.getByRole('heading', { level: 1, name: 'Apps' }),
      ).toBeVisible();
      await expect(page.getByRole('table', { name: 'Apps' })).toBeVisible();
    },
  },
  {
    name: 'app overview',
    path: '/apps/app-1/overview',
    ready: async (page) => {
      await expect(
        page.getByRole('heading', { level: 1, name: 'todo-app' }),
      ).toBeVisible();
      await expect(
        page.getByText('Live sessions', { exact: true }),
      ).toBeVisible();
    },
  },
  {
    name: 'permissions',
    path: '/apps/app-1/permissions',
    ready: async (page) => {
      await expect(
        page.getByRole('heading', { level: 1, name: 'Permissions' }),
      ).toBeVisible();
      await expect(
        page.getByRole('heading', { level: 2, name: 'todos' }),
      ).toBeVisible();
    },
  },
  {
    name: 'webhooks',
    path: '/apps/app-1/webhooks',
    ready: async (page) => {
      await expect(
        page.getByRole('heading', { level: 1, name: 'Webhooks' }),
      ).toBeVisible();
      await expect(page.getByRole('table', { name: 'Webhooks' })).toBeVisible();
    },
  },
  {
    name: 'backups',
    path: '/apps/app-1/backups',
    ready: async (page) => {
      await expect(
        page.getByRole('heading', { level: 1, name: 'Backups' }),
      ).toBeVisible();
      await expect(page.getByRole('table', { name: 'Backups' })).toBeVisible();
    },
  },
];

/** 200% and 400% zoom against the 1280px reference viewport. */
const ZOOM_WIDTHS = [
  { label: '200% zoom (640px)', width: 640 },
  { label: '320px width', width: 320 },
];

test.describe('reflow', () => {
  test.beforeEach(async ({ page }) => {
    await seedSession(page);
  });

  for (const pageCase of KEY_PAGES) {
    for (const zoom of ZOOM_WIDTHS) {
      test(`${pageCase.name} reflows at ${zoom.label}`, async ({ page }) => {
        await page.setViewportSize({ width: zoom.width, height: 900 });
        await page.goto(pageCase.path);
        await pageCase.ready(page);

        const metrics = await page.evaluate(() => ({
          documentWidth: document.documentElement.scrollWidth,
          viewportWidth: document.documentElement.clientWidth,
        }));

        expect(
          metrics.documentWidth,
          `${pageCase.name} must not scroll horizontally at ${String(zoom.width)}px`,
        ).toBeLessThanOrEqual(metrics.viewportWidth + 1);
      });
    }
  }
});
