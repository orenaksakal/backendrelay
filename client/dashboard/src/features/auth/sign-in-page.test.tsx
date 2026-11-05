import { screen, waitFor } from '@testing-library/react';
import userEvent, { type UserEvent } from '@testing-library/user-event';
import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import {
  readStoredSession,
  resetSession,
  SESSION_STORAGE_KEY,
} from '@/lib/auth/session';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import { mockSignInCode, mockSignInToken } from '@/test/fixtures/api/auth';
import publicConfig from '@/test/fixtures/api/config.json';
import { server } from '@/test/msw/node';
import { render } from '@/test/render';

const REDIRECT_ROUTE = '/login?redirect=%2Fapps%2Fapp-1%2Foverview';

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

async function renderLogin(route: string) {
  const result = render({ route });
  await screen.findByRole('heading', { name: 'Sign in to BackendRelay' });
  return result;
}

async function enterEmail(user: UserEvent): Promise<void> {
  const email = screen.getByLabelText('Email');
  await user.click(email);
  await user.type(email, 'owner@example.com');
  await user.click(screen.getByRole('button', { name: 'Send code' }));
  await screen.findByText(/We sent a 6-digit code to/);
}

describe('sign-in page', () => {
  it('signs in by keyboard alone and follows the redirect', async () => {
    const user = userEvent.setup();
    const { router } = await renderLogin(REDIRECT_ROUTE);

    // The email field takes focus on mount, so typing starts there.
    expect(screen.getByLabelText('Email')).toHaveFocus();
    await user.keyboard('owner@example.com');
    await user.keyboard('{Enter}');

    const code = await screen.findByLabelText('Code');
    await waitFor(() => {
      expect(code).toHaveFocus();
    });
    await user.keyboard(mockSignInCode);

    await waitFor(() => {
      expect(router.state.location.pathname).toBe('/apps/app-1/overview');
    });
    const session = readStoredSession(window.localStorage);
    expect(session?.token).toBe(mockSignInToken);
    expect(session?.user.email).toBe('owner@example.com');
  });

  it('accepts a pasted code with separators and submits at six digits', async () => {
    const user = userEvent.setup();
    const { router } = await renderLogin(REDIRECT_ROUTE);
    await enterEmail(user);

    const code = screen.getByLabelText('Code');
    await user.click(code);
    await user.paste('123 456');

    await waitFor(() => {
      expect(router.state.location.pathname).toBe('/apps/app-1/overview');
    });
  });

  it('announces an expired code on the field and keeps the session empty', async () => {
    server.use(
      http.post('/dash/auth/verify_magic_code', () =>
        HttpResponse.json(
          { type: 'record-expired', message: 'That code expired.' },
          { status: 400 },
        ),
      ),
    );

    const user = userEvent.setup();
    const { router } = await renderLogin(REDIRECT_ROUTE);
    await enterEmail(user);

    await user.type(screen.getByLabelText('Code'), '123456');

    expect(await screen.findByRole('alert')).toHaveTextContent(
      'That code expired. Send a new one.',
    );
    expect(screen.getByLabelText('Code')).toHaveAttribute(
      'aria-invalid',
      'true',
    );
    expect(router.state.location.pathname).toBe('/login');
    expect(window.localStorage.getItem(SESSION_STORAGE_KEY)).toBeNull();
  });

  it('reports closed sign-ups on the email field', async () => {
    server.use(
      http.post('/dash/auth/send_magic_code', () =>
        HttpResponse.json(
          { type: 'signups-closed', message: 'Sign-ups are closed.' },
          { status: 403 },
        ),
      ),
    );

    const user = userEvent.setup();
    await renderLogin('/login');

    const email = screen.getByLabelText('Email');
    await user.type(email, 'new@example.com');
    await user.click(screen.getByRole('button', { name: 'Send code' }));

    expect(await screen.findByRole('alert')).toHaveTextContent(
      'Sign-ups are closed on this deployment. Ask the operator for access.',
    );
    expect(email).toHaveAttribute('aria-invalid', 'true');
  });

  it('starts the resend countdown and keeps the email when going back', async () => {
    const user = userEvent.setup();
    await renderLogin('/login');
    await enterEmail(user);

    expect(
      await screen.findByRole('button', { name: /^Resend code in \d+s$/ }),
    ).toBeDisabled();

    await user.click(
      screen.getByRole('button', { name: 'Use a different email' }),
    );
    expect(screen.getByLabelText('Email')).toHaveValue('owner@example.com');
  });

  it('hides the Google button when the deployment does not configure it', async () => {
    await renderLogin('/login');

    expect(
      screen.queryByRole('link', { name: /Continue with Google/ }),
    ).not.toBeInTheDocument();
  });

  it('links the Google button to the management origin when configured', async () => {
    server.use(
      http.get('/dash/config', () =>
        HttpResponse.json({ ...publicConfig, googleLogin: true }),
      ),
    );

    await renderLogin(`${REDIRECT_ROUTE}&ticket=ticket-1`);

    const link = await screen.findByRole('link', {
      name: /Continue with Google/,
    });
    const url = new URL(link.getAttribute('href') ?? '');
    expect(url.origin).toBe('http://localhost:8888');
    expect(url.pathname).toBe('/dash/oauth/start');
    expect(url.searchParams.get('redirect_path')).toBe('/apps/app-1/overview');
    expect(url.searchParams.get('ticket')).toBe('ticket-1');
  });
});
