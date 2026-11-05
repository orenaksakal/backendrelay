import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { resetSession, setSession } from '@/lib/auth/session';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import { mockSignInToken, mockSignInUser } from '@/test/fixtures/api/auth';
import { mockTodoApp } from '@/test/fixtures/api/apps';
import {
  mockOauthClient,
  mockOauthClientSecretValue,
} from '@/test/fixtures/api/oauth-apps';
import { mockDash } from '@/test/msw/handlers';
import { server } from '@/test/msw/node';
import { render } from '@/test/render';

const API = 'http://localhost:8888';
const ROUTE = '/apps/app-1/oauth-apps/oauth-app-1';

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

describe('OAuth app detail', () => {
  it('saves the app info from the unsaved-changes bar', async () => {
    const user = userEvent.setup();
    let updated: unknown;
    server.use(
      http.post(
        `${API}/dash/apps/app-1/oauth-apps/oauth-app-1`,
        async ({ request }) => {
          updated = await request.json();
          return HttpResponse.json({
            app: {
              id: 'oauth-app-1',
              appId: 'app-1',
              appName: 'Acme Tools v2',
              appLogo: null,
              grantedScopes: [],
              isPublic: false,
              supportEmail: 'support@acme.test',
              appHomePage: 'https://acme.test',
              appPrivacyPolicyLink: 'https://acme.test/privacy',
              appTosLink: 'https://acme.test/terms',
              createdAt: '2026-01-10T10:00:00Z',
              updatedAt: '2026-03-01T10:00:00Z',
            },
          });
        },
      ),
    );

    render({ route: ROUTE });

    expect(
      await screen.findByRole('heading', { level: 1, name: 'Acme Tools' }),
    ).toBeInTheDocument();
    expect(screen.getByText(/This app is in test mode/)).toBeInTheDocument();

    const name = screen.getByLabelText('Name');
    expect(name).toHaveValue('Acme Tools');
    await user.clear(name);
    await user.type(name, 'Acme Tools v2');
    expect(screen.getByText('You have unsaved changes')).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => {
      expect(updated).toEqual({ app_name: 'Acme Tools v2' });
    });
    expect(await screen.findByText('OAuth app saved')).toBeInTheDocument();
    expect(screen.getByLabelText('Name')).toHaveValue('Acme Tools v2');
    expect(
      screen.getByRole('heading', { level: 1, name: 'Acme Tools v2' }),
    ).toBeInTheDocument();
  });

  it('creates a client secret, shows it once and deletes it', async () => {
    const user = userEvent.setup();
    render({ route: ROUTE });
    await screen.findByRole('heading', { level: 1, name: 'Acme Tools' });

    await user.click(screen.getByRole('button', { name: 'Acme CLI' }));
    const sheet = await screen.findByRole('dialog');
    expect(within(sheet).getByLabelText('Redirect URL 1')).toHaveValue(
      'https://acme.test/callback',
    );

    await user.click(
      within(sheet).getByRole('button', { name: 'Create secret' }),
    );
    expect(
      await screen.findByText(mockOauthClientSecretValue),
    ).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: "I've saved it" }));

    expect(await screen.findByText('9z8y••••••••')).toBeInTheDocument();

    const secretRow = screen.getByText('9z8y••••••••').closest('li');
    expect(secretRow).not.toBeNull();
    if (secretRow === null) return;
    await user.click(within(secretRow).getByRole('button', { name: 'Delete' }));

    const heading = await screen.findByRole('heading', {
      name: 'Delete client secret?',
    });
    const confirm = heading.closest('[role="dialog"]');
    expect(confirm).not.toBeNull();
    if (!(confirm instanceof HTMLElement)) return;
    await user.click(
      within(confirm).getByRole('button', { name: 'Delete client secret' }),
    );

    await waitFor(() => {
      expect(screen.queryByText('9z8y••••••••')).not.toBeInTheDocument();
    });
    await user.click(within(sheet).getByRole('button', { name: 'Cancel' }));
  });

  it('adds a redirect URL and saves it', async () => {
    const user = userEvent.setup();
    const bodies: unknown[] = [];
    server.use(
      http.post(
        `${API}/dash/apps/app-1/oauth-app-clients/oauth-client-1`,
        async ({ request }) => {
          bodies.push(await request.json());
          return HttpResponse.json({
            client: {
              ...mockOauthClient,
              authorizedRedirectUrls: [
                ...mockOauthClient.authorizedRedirectUrls,
                'https://acme.test/new',
              ],
            },
          });
        },
      ),
    );

    render({ route: ROUTE });
    await screen.findByRole('heading', { level: 1, name: 'Acme Tools' });

    await user.click(screen.getByRole('button', { name: 'Acme CLI' }));
    const sheet = await screen.findByRole('dialog');
    await user.click(
      within(sheet).getByRole('button', { name: 'Add redirect URL' }),
    );
    await user.type(
      within(sheet).getByLabelText('Redirect URL 3'),
      'https://acme.test/new',
    );
    await user.click(within(sheet).getByRole('button', { name: 'Save' }));

    await waitFor(() => {
      expect(bodies).toEqual([
        { app_id: 'app-1', add_redirect_url: 'https://acme.test/new' },
      ]);
    });
    expect(await screen.findByText('OAuth client saved')).toBeInTheDocument();
    expect(screen.getByText('https://acme.test/new')).toBeInTheDocument();
  });

  it('hides the delete action from collaborators', async () => {
    server.use(
      http.get(`${API}/dash`, () =>
        HttpResponse.json({
          ...mockDash,
          apps: [{ ...mockTodoApp, role: 'collaborator' }],
        }),
      ),
    );

    render({ route: ROUTE });
    await screen.findByRole('heading', { level: 1, name: 'Acme Tools' });
    expect(
      screen.queryByRole('button', { name: 'Delete OAuth app' }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole('button', { name: 'Delete' }),
    ).not.toBeInTheDocument();
  });
});
