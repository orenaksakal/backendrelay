import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import {
  readStoredSession,
  resetSession,
  setSession,
} from '@/lib/auth/session';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import { mockSignInToken, mockSignInUser } from '@/test/fixtures/api/auth';
import {
  mockConsent,
  mockConsentRedirectId,
} from '@/test/fixtures/api/identity';
import { server } from '@/test/msw/node';
import { render } from '@/test/render';

const CONSENT = `/platform/oauth/start?redirect-id=${mockConsentRedirectId}`;

beforeEach(() => {
  resetSession();
  resetRuntimeConfig();
  window.localStorage.clear();
});

afterEach(() => {
  resetSession();
  resetRuntimeConfig();
  window.localStorage.clear();
});

function signIn(): void {
  setSession({ token: mockSignInToken, user: mockSignInUser });
}

describe('OAuth consent', () => {
  it('shows the sign-in card when signed out and returns to the request', async () => {
    const { router } = render({ route: CONSENT });

    expect(
      await screen.findByRole('heading', { name: 'Sign in to BackendRelay' }),
    ).toBeInTheDocument();
    expect(router.state.location.pathname).toBe('/platform/oauth/start');
    expect(router.state.location.search).toMatchObject({
      'redirect-id': mockConsentRedirectId,
    });
  });

  it('shows the app, its scopes and where it will redirect', async () => {
    signIn();
    render({ route: CONSENT });

    expect(
      await screen.findByRole('heading', {
        name: `${mockConsent.appName} wants to access your BackendRelay account`,
      }),
    ).toBeInTheDocument();
    expect(screen.getByText(mockConsent.userEmail)).toBeInTheDocument();
    expect(
      screen.getByText(/Read access to all of your BackendRelay apps/),
    ).toBeInTheDocument();
    expect(
      screen.getByText(
        /Query all of the data in any of your BackendRelay databases/,
      ),
    ).toBeInTheDocument();
    expect(screen.getByText('https://client.test')).toBeInTheDocument();
  });

  it('posts the redirect id and grant token to both decisions', async () => {
    signIn();
    render({ route: CONSENT });

    const allow = await screen.findByRole('button', { name: 'Allow' });
    const deny = screen.getByRole('button', { name: 'Deny' });
    const allowForm = allow.closest('form');
    const denyForm = deny.closest('form');

    expect(allowForm?.method).toBe('post');
    expect(allowForm?.getAttribute('action')).toContain(
      '/platform/oauth/grant',
    );
    expect(denyForm?.method).toBe('post');
    expect(denyForm?.getAttribute('action')).toContain('/platform/oauth/deny');

    for (const form of [allowForm, denyForm]) {
      expect(
        form?.querySelector<HTMLInputElement>('input[name="redirect_id"]')
          ?.value,
      ).toBe(mockConsentRedirectId);
      expect(
        form?.querySelector<HTMLInputElement>('input[name="grant_token"]')
          ?.value,
      ).toBe(mockConsent.grantToken);
    }
  });

  it('signs out from "Not you?" without leaving the consent screen', async () => {
    const user = userEvent.setup();
    signIn();
    render({ route: CONSENT });

    await user.click(
      await screen.findByRole('button', { name: 'Not you? Sign out' }),
    );

    expect(
      await screen.findByRole('heading', { name: 'Sign in to BackendRelay' }),
    ).toBeInTheDocument();
    expect(readStoredSession(window.localStorage)).toBeNull();
  });

  it('reports a claim the server rejects', async () => {
    server.use(
      http.post('/platform/oauth/claim', () =>
        HttpResponse.json(
          { type: 'record-expired', message: 'This request expired.' },
          { status: 400 },
        ),
      ),
    );
    signIn();
    render({ route: CONSENT });

    expect(
      await screen.findByRole('heading', {
        name: "This consent link isn't valid",
      }),
    ).toBeInTheDocument();
    expect(screen.getByRole('alert')).toHaveTextContent(
      'This request expired.',
    );
  });

  it('rejects a link that has no redirect id', async () => {
    signIn();
    render({ route: '/platform/oauth/start' });

    await waitFor(() => {
      expect(
        screen.getByRole('heading', {
          name: "This consent link isn't valid",
        }),
      ).toBeInTheDocument();
    });
    expect(screen.getByRole('alert')).toHaveTextContent(
      'The request is missing its redirect ID.',
    );
  });
});
