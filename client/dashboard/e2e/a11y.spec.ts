import type { Page } from '@playwright/test';
import {
  mockCliTicket,
  mockConsentRedirectId,
  mockOAuthCode,
} from '../src/test/fixtures/api/identity';
import { expect, test } from './fixtures';

/**
 * D9.1 accessibility sweep: every navigable route renders with the mocked API
 * and passes axe in both colour schemes. Pathless layouts and redirect-only
 * helpers are covered through their destinations. The per-screen specs cover
 * their own flows in depth; this spec is the safety net that catches a screen
 * that was missed or a theme regression in the shared chrome.
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

function expectHeading(page: Page, name: string): Promise<void> {
  return expect(page.getByRole('heading', { level: 1, name })).toBeVisible();
}

function expectSubheading(page: Page, name: string): Promise<void> {
  return expect(page.getByRole('heading', { level: 2, name })).toBeVisible();
}

interface RouteCase {
  path: string;
  ready: (page: Page) => Promise<void>;
}

const CONSENT_PATH = `/platform/oauth/start?redirect-id=${mockConsentRedirectId}`;

/**
 * Axe runs once per colour scheme. Playwright's projects already set one
 * preference, but toggling here keeps both scans inside every project instead
 * of depending on the project matrix.
 */
async function scanBothThemes(
  page: Page,
  checkA11y: (exclude?: string[]) => Promise<void>,
): Promise<void> {
  for (const scheme of ['light', 'dark'] as const) {
    // Keep transitions disabled while the scan compares computed colours; a
    // theme switch otherwise samples the previous colour mid-transition.
    await page.emulateMedia({
      colorScheme: scheme,
      reducedMotion: 'reduce',
    });
    if (scheme === 'dark') {
      await expect(page.locator('html')).toHaveClass(/dark/);
    } else {
      await expect(page.locator('html')).not.toHaveClass(/dark/);
    }
    // Let the provider's theme render and any colour transitions settle before
    // axe samples the page.
    await page.evaluate(
      () =>
        new Promise<void>((resolve) => {
          requestAnimationFrame(() => {
            requestAnimationFrame(() => {
              resolve();
            });
          });
        }),
    );
    await page.waitForTimeout(250);
    await checkA11y();
  }
}

const PUBLIC_ROUTES: RouteCase[] = [
  {
    path: '/login',
    ready: async (page) => {
      await expectHeading(page, 'Sign in to BackendRelay');
    },
  },
  {
    path: `/dash/oauth/callback?code=${mockOAuthCode}`,
    ready: async (page) => {
      await expectHeading(page, 'todo-app');
    },
  },
  {
    path: '/dash/oauth/callback?error=access_denied',
    ready: async (page) => {
      await expectHeading(page, 'Google sign-in failed');
    },
  },
  {
    path: CONSENT_PATH,
    ready: async (page) => {
      await expectHeading(page, 'Sign in to BackendRelay');
    },
  },
  {
    path: '/',
    ready: async (page) => {
      await expect(page).toHaveURL(/\/login\?redirect=/);
      await expectHeading(page, 'Sign in to BackendRelay');
    },
  },
  {
    path: '/apps/app-1/overview',
    ready: async (page) => {
      await expect(page).toHaveURL(/\/login\?redirect=/);
    },
  },
];

const AUTHED_ROUTES: RouteCase[] = [
  {
    path: `/cli/approve?ticket=${mockCliTicket}`,
    ready: async (page) => {
      await expectHeading(page, 'Sign in to the BackendRelay CLI?');
    },
  },
  {
    path: CONSENT_PATH,
    ready: async (page) => {
      await expect(page.getByRole('button', { name: 'Allow' })).toBeVisible();
    },
  },
  {
    path: '/',
    ready: async (page) => {
      await expect(page).toHaveURL(/\/w\/personal(?:\?.*)?$/);
      await expectHeading(page, 'Apps');
    },
  },
  {
    path: '/w/personal',
    ready: async (page) => {
      await expectHeading(page, 'Apps');
      await expect(page.getByRole('table', { name: 'Apps' })).toBeVisible();
    },
  },
  {
    path: '/w/org-1',
    ready: async (page) => {
      await expectHeading(page, 'Apps');
    },
  },
  {
    path: '/w/org-1/members',
    ready: async (page) => {
      await expectHeading(page, 'Members');
    },
  },
  {
    path: '/w/org-1/settings',
    ready: async (page) => {
      await expectHeading(page, 'Settings');
    },
  },
  {
    path: '/new',
    ready: async (page) => {
      await expectHeading(page, 'Create app');
    },
  },
  {
    path: '/dash/new',
    ready: async (page) => {
      await expect(page).toHaveURL(/\/new$/);
      await expectHeading(page, 'Create app');
    },
  },
  {
    path: '/dash',
    ready: async (page) => {
      await expect(page).toHaveURL(/\/w\/personal(?:\?.*)?$/);
      await expectHeading(page, 'Apps');
    },
  },
  {
    path: '/welcome',
    ready: async (page) => {
      await expectHeading(page, 'Create your first app');
    },
  },
  {
    path: '/account',
    ready: async (page) => {
      await expect(page).toHaveURL(/\/account\/profile$/);
      await expectHeading(page, 'Profile');
    },
  },
  {
    path: '/account/profile',
    ready: async (page) => {
      await expectHeading(page, 'Profile');
    },
  },
  {
    path: '/account/tokens',
    ready: async (page) => {
      await expectHeading(page, 'Tokens');
    },
  },
  {
    path: '/account/invites',
    ready: async (page) => {
      await expectHeading(page, 'Invites');
    },
  },
  {
    path: '/account/oauth-apps',
    ready: async (page) => {
      await expectHeading(page, 'OAuth apps');
    },
  },
  {
    path: '/apps/app-1',
    ready: async (page) => {
      await expect(page).toHaveURL(/\/apps\/app-1\/overview$/);
      await expectHeading(page, 'todo-app');
    },
  },
  {
    path: '/apps/app-1/overview',
    ready: async (page) => {
      await expectHeading(page, 'todo-app');
    },
  },
  {
    path: '/apps/app-1/explorer',
    ready: async (page) => {
      await expectHeading(page, 'Explorer');
    },
  },
  {
    path: '/apps/app-1/schema',
    ready: async (page) => {
      await expectHeading(page, 'Schema');
    },
  },
  {
    path: '/apps/app-1/users',
    ready: async (page) => {
      await expectHeading(page, 'Users');
    },
  },
  {
    path: '/apps/app-1/storage',
    ready: async (page) => {
      await expectHeading(page, 'Storage');
    },
  },
  {
    path: '/apps/app-1/permissions',
    ready: async (page) => {
      await expectHeading(page, 'Permissions');
      await expectSubheading(page, 'todos');
    },
  },
  {
    path: '/apps/app-1/auth',
    ready: async (page) => {
      await expectHeading(page, 'Auth');
    },
  },
  {
    path: '/apps/app-1/playground',
    ready: async (page) => {
      await expectHeading(page, 'Playground');
    },
  },
  {
    path: '/apps/app-1/webhooks',
    ready: async (page) => {
      await expectHeading(page, 'Webhooks');
      await expect(page.getByRole('table', { name: 'Webhooks' })).toContainText(
        'https://example.com/hooks/instant',
      );
    },
  },
  {
    path: '/apps/app-1/webhooks/webhook-1',
    ready: async (page) => {
      await expectHeading(page, 'https://example.com/hooks/instant');
    },
  },
  {
    path: '/apps/app-1/backups',
    ready: async (page) => {
      await expectHeading(page, 'Backups');
      await expect(page.getByRole('table', { name: 'Backups' })).toBeVisible();
    },
  },
  {
    path: '/apps/app-1/oauth-apps',
    ready: async (page) => {
      await expectHeading(page, 'OAuth apps');
    },
  },
  {
    path: '/apps/app-1/oauth-apps/oauth-app-1',
    ready: async (page) => {
      await expectHeading(page, 'Acme Tools');
    },
  },
  {
    path: '/apps/app-1/settings',
    ready: async (page) => {
      await expectHeading(page, 'Settings');
    },
  },
  {
    path: '/operator/settings',
    ready: async (page) => {
      await expectHeading(page, 'Operator settings');
    },
  },
  {
    path: '/operator/restores',
    ready: async (page) => {
      await expectHeading(page, 'Restores');
    },
  },
  {
    path: '/intern/deployment-settings',
    ready: async (page) => {
      await expect(page).toHaveURL(/\/operator\/settings$/);
      await expectHeading(page, 'Operator settings');
    },
  },
  {
    path: '/_devtool?appId=app-1',
    ready: async (page) => {
      await expect(page.getByText('todo-app', { exact: true })).toBeVisible();
    },
  },
  {
    path: '/_dev/catalog',
    ready: async (page) => {
      await expectHeading(page, 'Component catalog');
      // Skeletons start after 150ms; wait so the scan sees the final layouts.
      await page.waitForTimeout(400);
    },
  },
];

test.describe('accessibility sweep: signed out', () => {
  for (const route of PUBLIC_ROUTES) {
    test(`${route.path} is clean in light and dark`, async ({
      page,
      checkA11y,
    }) => {
      await page.goto(route.path);
      await route.ready(page);
      await scanBothThemes(page, checkA11y);
    });
  }
});

test.describe('accessibility sweep: signed in', () => {
  test.beforeEach(async ({ page }) => {
    await seedSession(page);
  });

  for (const route of AUTHED_ROUTES) {
    test(`${route.path} is clean in light and dark`, async ({
      page,
      checkA11y,
    }) => {
      await page.goto(route.path);
      await route.ready(page);
      await scanBothThemes(page, checkA11y);
    });
  }
});
