import { screen, waitFor } from '@testing-library/react';
import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { readStoredSession, resetSession } from '@/lib/auth/session';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import {
  mockCliTicket,
  mockOAuthCode,
  mockOAuthToken,
} from '@/test/fixtures/api/identity';
import { server } from '@/test/msw/node';
import { render } from '@/test/render';

const CALLBACK = `/dash/oauth/callback?code=${mockOAuthCode}`;

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

describe('Google callback', () => {
  it('exchanges the code, stores the session and follows redirect_path', async () => {
    const { router } = render({ route: CALLBACK });

    await waitFor(() => {
      expect(router.state.location.pathname).toBe('/apps/app-1/overview');
    });
    expect(readStoredSession(window.localStorage)?.token).toBe(mockOAuthToken);
  });

  it('goes to CLI approval when the sign-in carried a ticket', async () => {
    const { router } = render({
      route: `${CALLBACK}&ticket=${mockCliTicket}`,
    });

    await waitFor(() => {
      expect(router.state.location.pathname).toBe('/cli/approve');
    });
    expect(router.state.location.search).toMatchObject({
      ticket: mockCliTicket,
    });
    expect(readStoredSession(window.localStorage)?.token).toBe(mockOAuthToken);
  });

  it('shows the provider error and offers a way back to sign in', async () => {
    render({ route: '/dash/oauth/callback?error=access_denied' });

    expect(
      await screen.findByRole('heading', { name: 'Google sign-in failed' }),
    ).toBeInTheDocument();
    expect(screen.getByRole('alert')).toHaveTextContent('access_denied');
    expect(
      screen.getByRole('link', { name: 'Back to sign in' }),
    ).toHaveAttribute('href', '/login');
    expect(readStoredSession(window.localStorage)).toBeNull();
  });

  it('shows the exchange failure instead of signing in', async () => {
    server.use(
      http.post('/dash/oauth/token', () =>
        HttpResponse.json(
          { type: 'record-expired', message: 'That sign-in link expired.' },
          { status: 400 },
        ),
      ),
    );

    render({ route: CALLBACK });

    expect(await screen.findByRole('alert')).toHaveTextContent(
      'That sign-in link expired.',
    );
    expect(readStoredSession(window.localStorage)).toBeNull();
  });

  it('sends a visit without code or error back to sign in', async () => {
    const { router } = render({ route: '/dash/oauth/callback' });

    await waitFor(() => {
      expect(router.state.location.pathname).toBe('/login');
    });
    expect(
      await screen.findByRole('heading', { name: 'Sign in to BackendRelay' }),
    ).toBeInTheDocument();
  });
});
