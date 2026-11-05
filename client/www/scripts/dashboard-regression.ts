import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import type { DashResponse, InstantApp } from '../lib/types';

const require = createRequire(import.meta.url);
const {
  [process.env.INSTANT_DASHBOARD_BROWSER || 'chromium']: engine,
} = require('../../packages/core/node_modules/playwright');
const base = process.env.BASE_URL || 'http://localhost:3002';
const appId = '123e4567-e89b-12d3-a456-426614174000';
const secondId = '123e4567-e89b-12d3-a456-426614174001';
const removedRequests: string[] = [];
const orgId = '123e4567-e89b-12d3-a456-426614174002';
const makeApp = (id: string, title: string): InstantApp => ({
  id,
  title,
  admin_token: `fixture-${id}`,
  pro: false,
  created_at: '2026-01-01T00:00:00Z',
  rules: null,
  rules_version: null,
  user_app_role: 'owner',
  members: [],
  invites: [],
  org: null,
  webhooks: [],
  magic_code_email_template: null,
  magic_code_expiry_minutes: null,
});
const personal = makeApp(appId, 'Personal app');
const second = makeApp(secondId, 'Second app');
const org = {
  id: orgId,
  title: 'Test organization',
  role: 'collaborator' as const,
  paid: false,
  created_at: '2026-01-01T00:00:00Z',
};
const dash: DashResponse = {
  apps: [personal, second],
  superuser: false,
  invites: [],
  orgs: [org],
  user: { id: 'fixture-user', email: 'member@example.test' },
};
const tabs = [
  ['home', 'Home', 'Getting Started'],
  ['explorer', 'Explorer', 'Namespaces'],
  ['schema', 'Schema', 'This is the schema for your app in code form.'],
  ['perms', 'Permissions', 'Ready to share your app with the world?'],
  ['auth', 'Auth', 'Social login'],
  ['webhooks', 'Webhooks', 'Webhooks'],
  ['backups', 'Backups', 'Point-in-time snapshots'],
  ['repl', 'Query Inspector', ''],
  ['sandbox', 'Sandbox', 'Debug mode.'],
  ['admin', 'Admin', 'App settings'],
  ['oauth-apps', 'OAuth Apps', 'OAuth Apps'],
] as const;

async function fixtures(page: any) {
  let schedule: {
    cron: string;
    enabled: boolean;
    next_run_at: string;
    last_enqueued_at: null;
  } | null = null;
  // All API and realtime traffic is intercepted. Never use a real session or
  // send fixture credentials to a running development/production backend.
  await page.route(/\/dash(?:\/|\?|$)/, async (route: any) => {
    const url = new URL(route.request().url());
    if (url.origin === new URL(base).origin) return route.continue();
    const path = url.pathname;
    let body: unknown;
    if (path === '/dash') body = dash;
    else if (path === `/dash/orgs/${orgId}`)
      body = {
        org,
        id: orgId,
        apps: [{ ...second, user_app_role: 'collaborator' }],
        members: [],
        invites: [],
      };
    else if (path.endsWith('/stats'))
      body = { count: 3, origins: { 'https://example.test': 3 } };
    else if (path.endsWith('/auth'))
      body = {
        oauth_service_providers: [],
        oauth_clients: [],
        authorized_redirect_origins: [],
      };
    else if (path.endsWith('/rule-versions')) body = { versions: [] };
    else if (path.endsWith('/backups')) body = { backups: [] };
    else if (path.endsWith('/backup-jobs')) body = { jobs: [] };
    else if (path.endsWith('/backup-schedule')) {
      if (route.request().method() === 'PUT') {
        assert.equal(
          route.request().headers().authorization,
          'Bearer fixture-dashboard-token',
        );
        schedule = {
          ...route.request().postDataJSON(),
          next_run_at: '2026-11-01T00:00:00Z',
          last_enqueued_at: null,
        };
      }
      body = { schedule };
    } else if (path.endsWith('/oauth-apps')) body = { apps: [] };
    else if (path.endsWith('/personal_access_tokens')) body = { data: [] };
    else if (path.endsWith('/oauth_apps')) body = { oauthApps: [] };
    else if (path.endsWith('/webhooks')) body = { webhooks: [] };
    else if (path.endsWith('/logs') || path.includes('/execution/')) {
      removedRequests.push(path);
      await route.fulfill({ status: 404, body: '{}' });
      return;
    } else body = {};
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify(body),
    });
  });
  await page.routeWebSocket(/\/runtime\/session/, (socket: any) => {
    socket.onMessage((raw: string) => {
      const message = JSON.parse(raw);
      if (message.op === 'init')
        socket.send(
          JSON.stringify({
            op: 'init-ok',
            'client-event-id': message['client-event-id'],
            'session-id': 'fixture-session',
            attrs: [],
          }),
        );
      if (message.op === 'add-query')
        socket.send(
          JSON.stringify({
            op: 'add-query-ok',
            'client-event-id': message['client-event-id'],
            q: message.q,
            result: [],
            attrs: [],
          }),
        );
    });
  });
}

async function createPage(
  browser: any,
  width: number,
  dark: boolean,
  authenticated = true,
) {
  const context = await browser.newContext({
    viewport: { width, height: 900 },
  });
  await context.addInitScript(
    ({ authenticated, dark }: { authenticated: boolean; dark: boolean }) => {
      localStorage.setItem('devBackend', 'true');
      // Do not re-apply the initial theme/workspace on reload or history changes.
      if (!sessionStorage.getItem('fixture-initialized')) {
        localStorage.setItem('darkMode', String(dark));
        localStorage.setItem('workspace', 'personal');
        sessionStorage.setItem('fixture-initialized', 'true');
      }
      if (authenticated)
        localStorage.setItem(
          '@AUTH',
          JSON.stringify({
            token: 'fixture-dashboard-token',
            user: {
              id: 'fixture-user',
              email: 'member@example.test',
              created_at: '2026-01-01T00:00:00Z',
            },
          }),
        );
    },
    { authenticated, dark },
  );
  const page = await context.newPage();
  await fixtures(page);
  return { page, context };
}

async function assertLayout(page: any) {
  assert.equal(
    await page.evaluate(
      () => document.documentElement.scrollWidth > innerWidth + 1,
    ),
    false,
    'document must not overflow horizontally',
  );
  const content = await page.locator('#dashboard-page-content').boundingBox();
  assert.ok(
    content && content.width > 0 && content.height > 100,
    'workspace must remain usable',
  );
}

async function run() {
  const browser = await engine.launch({ headless: true });
  const errors: string[] = [];
  let diagnosticPage: any;
  try {
    const anonymous = await createPage(browser, 1280, false, false);
    await anonymous.page.goto(`${base}/dash`);
    await anonymous.page.getByLabel('Email address', { exact: true }).waitFor();
    assert.equal(
      await anonymous.page.locator('.console-auth-intro').count(),
      0,
    );
    await anonymous.context.close();
    for (const width of [1280, 375])
      for (const dark of [false, true]) {
        const { page, context } = await createPage(browser, width, dark);
        diagnosticPage = page;
        page.on('pageerror', (error: Error) => {
          if (
            !/Could not find source file:|websocket|backend connection/i.test(
              error.message,
            )
          )
            errors.push(error.message);
        });
        await page.goto(`${base}/dash?app=${appId}&t=home`);
        await page.getByText('Getting Started', { exact: true }).waitFor();
        const navigation = page.getByRole('navigation', {
          name: 'Application navigation',
        });
        assert.deepEqual(
          await navigation
            .locator(':scope > div:first-child > a')
            .allTextContents(),
          tabs.map(([, label]) => label),
        );
        assert.equal(await navigation.getByText('Rust tools').count(), 0);
        for (const label of [
          'Tasks',
          'Workflows',
          'Schedules',
          'Deployments',
          'Secrets',
          'Insights',
          'Logs',
        ]) {
          assert.equal(
            await navigation
              .getByRole('link', { name: label, exact: true })
              .count(),
            0,
          );
        }
        assert.equal(
          await page
            .locator('.console-context-tabs, .console-nav-group-heading')
            .count(),
          0,
          'redesigned navigation must not be mounted',
        );
        await page
          .getByText('sessions are connected right now', { exact: false })
          .waitFor();
        for (const [id, label, expected] of tabs) {
          await navigation
            .getByRole('link', { name: label, exact: true })
            .click();
          await page.waitForFunction(
            (id: string) => new URL(location.href).searchParams.get('t') === id,
            id,
          );
          await page.locator('#dashboard-page-content').waitFor();
          if (id === 'explorer' && width < 768) {
            await page
              .getByRole('button', { name: 'Open namespaces', exact: true })
              .click();
            await page
              .locator('#dashboard-page-content')
              .getByText('Namespaces', { exact: true })
              .waitFor();
            await page
              .getByRole('button', { name: 'Close namespaces', exact: true })
              .click();
          } else if (expected)
            await page
              .locator('#dashboard-page-content')
              .getByText(expected, { exact: false })
              .first()
              .waitFor();
          await assertLayout(page);
          assert.equal(
            await navigation
              .getByRole('link', { name: label, exact: true })
              .getAttribute('aria-current'),
            'page',
          );
        }
        await navigation
          .getByRole('link', { name: 'Schema', exact: true })
          .click();
        await page.goBack();
        await page.waitForFunction(
          () => new URL(location.href).searchParams.get('t') === 'oauth-apps',
        );
        await page.goForward();
        await page.waitForFunction(
          () => new URL(location.href).searchParams.get('t') === 'schema',
        );
        for (const tab of [
          'execution',
          'workflows',
          'schedules',
          'deployments',
          'secrets',
          'execution-limits',
          'execution-audit',
          'insights',
          'logs',
        ]) {
          await page.goto(`${base}/dash?app=${appId}&t=${tab}`);
          await page.getByText('Getting Started', { exact: true }).waitFor();
          assert.equal(
            await navigation
              .getByRole('link', { name: 'Home', exact: true })
              .getAttribute('aria-current'),
            'page',
          );
          await assertLayout(page);
        }
        await navigation
          .getByRole('link', { name: 'Backups', exact: true })
          .click();
        await page
          .getByText('Automatic backup schedule', { exact: true })
          .click();
        await page
          .getByRole('button', { name: 'Configure schedule', exact: true })
          .click();
        await page
          .getByRole('button', { name: 'Save schedule', exact: true })
          .click();
        await page
          .getByText('Backup schedule saved.', { exact: true })
          .waitFor();
        await page
          .getByRole('button', { name: 'Pause schedule', exact: true })
          .click();
        await page
          .getByText('Backup schedule paused.', { exact: true })
          .waitFor();
        await page
          .getByRole('button', { name: 'Resume schedule', exact: true })
          .click();
        await page
          .getByText('Backup schedule saved.', { exact: true })
          .waitFor();
        await navigation
          .getByRole('link', { name: 'Schema', exact: true })
          .click();
        await page
          .getByLabel(dark ? 'Switch to light mode' : 'Switch to dark mode')
          .click();
        await page.reload();
        await navigation.waitFor();
        assert.equal(
          await page
            .locator('html')
            .evaluate((element: Element) => element.classList.contains('dark')),
          !dark,
        );
        await page.getByRole('button', { name: 'Show applications' }).click();
        await page
          .getByRole('button', { name: 'Pin Second app', exact: true })
          .click();
        await page
          .getByRole('button', { name: 'Unpin Second app', exact: true })
          .waitFor();
        await page
          .getByRole('combobox', { name: 'Switch application' })
          .fill('sec');
        await page.getByRole('option', { name: 'Second app' }).click();
        await page.waitForFunction(
          (id: string) => new URL(location.href).searchParams.get('app') === id,
          secondId,
        );
        assert.equal(
          await page
            .getByRole('combobox', { name: 'Switch application' })
            .inputValue(),
          'Second app',
        );
        await page.getByRole('button', { name: 'Switch workspace' }).click();
        await page
          .getByRole('button', { name: 'Test organization', exact: true })
          .click();
        await page.waitForFunction(
          (id: string) => new URL(location.href).searchParams.get('org') === id,
          orgId,
        );
        assert.equal(
          await navigation
            .getByRole('link', { name: 'Admin', exact: true })
            .count(),
          0,
          'collaborators must not see Admin',
        );
        await page.goto(`${base}/dash?app=${secondId}&org=${orgId}&t=admin`);
        await page
          .getByText('This page is unavailable for your role', { exact: true })
          .waitFor();
        await page.getByRole('button', { name: 'Switch workspace' }).click();
        await page
          .getByRole('button', { name: 'member@example.test', exact: true })
          .click();
        await page.getByRole('link', { name: 'New app', exact: true }).click();
        await page.getByText('Time for a new app?', { exact: true }).waitFor();
        await page
          .getByRole('button', { name: 'Nevermind', exact: true })
          .click();
        await navigation.waitFor();
        await page.goto(`${base}/dash/user-settings?org=personal`);
        await page.getByText('User Settings', { exact: true }).waitFor();
        await page.getByText('Access Tokens', { exact: true }).waitFor();
        await page.goto(`${base}/dash?app=${appId}&t=home&org=personal`);
        await page.getByText('Getting Started', { exact: true }).waitFor();
        await page
          .getByLabel(dark ? 'Switch to dark mode' : 'Switch to light mode')
          .click();
        if (process.env.SCREENSHOT_PATH)
          await page.screenshot({
            path: process.env.SCREENSHOT_PATH.replace(
              /(\.[^.]+)?$/,
              `-${width}-${dark ? 'dark' : 'light'}$1`,
            ),
            fullPage: true,
          });
        await context.close();
        console.log(
          `Original dashboard: ${width}px ${dark ? 'dark' : 'light'} passed`,
        );
      }
    assert.deepEqual(
      removedRequests,
      [],
      'dashboard must never request removed feature APIs',
    );
    assert.deepEqual(
      errors,
      [],
      'dashboard should not raise browser exceptions',
    );
  } catch (error) {
    if (diagnosticPage && !diagnosticPage.isClosed()) {
      console.error(
        'Browser diagnostics:',
        errors,
        await diagnosticPage.locator('body').innerText(),
      );
      if (process.env.SCREENSHOT_PATH)
        await diagnosticPage.screenshot({
          path: process.env.SCREENSHOT_PATH.replace(
            /(\.[^.]+)?$/,
            '-failure$1',
          ),
          fullPage: true,
        });
    }
    throw error;
  } finally {
    await browser.close();
  }
}

run().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
