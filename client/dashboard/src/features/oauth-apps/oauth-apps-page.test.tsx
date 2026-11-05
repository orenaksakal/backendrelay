import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { resetSession, setSession } from '@/lib/auth/session';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import { mockSignInToken, mockSignInUser } from '@/test/fixtures/api/auth';
import { mockTodoApp } from '@/test/fixtures/api/apps';
import { mockDash } from '@/test/msw/handlers';
import { server } from '@/test/msw/node';
import { render } from '@/test/render';

const API = 'http://localhost:8888';
const ROUTE = '/apps/app-1/oauth-apps';

beforeEach(() => {
  resetSession();
  resetRuntimeConfig();
  window.localStorage.clear();
  setSession({ token: mockSignInToken, user: mockSignInUser });
});

afterEach(() => {
  resetSession();
  resetRuntimeConfig();
  window.localStorage.clear();
});

describe('OAuth app list', () => {
  it('lists the OAuth apps with their client counts', async () => {
    render({ route: ROUTE });

    expect(
      await screen.findByRole('heading', { level: 1, name: 'OAuth apps' }),
    ).toBeInTheDocument();
    const table = await screen.findByRole('table', { name: 'OAuth apps' });
    expect(within(table).getByText('Acme Tools')).toBeInTheDocument();
    expect(within(table).getByText('Docs portal')).toBeInTheDocument();

    const row = within(table).getByText('Acme Tools').closest('tr');
    expect(row).not.toBeNull();
    if (row === null) return;
    expect(within(row).getByText('1')).toBeInTheDocument();
  });

  it('creates an OAuth app and opens its detail page', async () => {
    const user = userEvent.setup();
    let created: unknown;
    server.use(
      http.post(`${API}/dash/apps/app-1/oauth-apps`, async ({ request }) => {
        created = await request.json();
        return HttpResponse.json({
          app: {
            id: 'oauth-app-created',
            appId: 'app-1',
            appName: 'Docs bot',
            appLogo: null,
            grantedScopes: [],
            isPublic: false,
            supportEmail: null,
            appHomePage: null,
            appPrivacyPolicyLink: null,
            appTosLink: null,
            createdAt: '2026-03-01T10:00:00Z',
            updatedAt: '2026-03-01T10:00:00Z',
            clients: [],
          },
        });
      }),
    );

    const { router } = render({ route: ROUTE });
    await screen.findByRole('heading', { level: 1, name: 'OAuth apps' });

    await user.click(screen.getByRole('button', { name: 'Create OAuth app' }));
    const sheet = await screen.findByRole('dialog');
    await user.type(within(sheet).getByLabelText('Name'), 'Docs bot');
    await user.click(
      within(sheet).getByRole('button', { name: 'Create OAuth app' }),
    );

    await waitFor(() => {
      expect(created).toEqual({
        app_id: 'app-1',
        app_name: 'Docs bot',
        app_logo: null,
        app_home_page: null,
        app_privacy_policy_link: null,
        app_tos_link: null,
        support_email: null,
      });
    });
    await waitFor(() => {
      expect(router.state.location.pathname).toBe(
        '/apps/app-1/oauth-apps/oauth-app-created',
      );
    });
    expect(
      await screen.findByRole('heading', { level: 1, name: 'Docs bot' }),
    ).toBeInTheDocument();
  });

  it('refuses the page to app members', async () => {
    server.use(
      http.get(`${API}/dash`, () =>
        HttpResponse.json({
          ...mockDash,
          apps: [{ ...mockTodoApp, role: 'app-member' }],
        }),
      ),
    );

    render({ route: ROUTE });

    expect(
      await screen.findByText("You don't have access to OAuth apps"),
    ).toBeInTheDocument();
    expect(screen.getByText(/needs the collaborator role/)).toBeInTheDocument();
  });
});
