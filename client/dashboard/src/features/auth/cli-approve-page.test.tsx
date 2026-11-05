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
import { mockCliTicket } from '@/test/fixtures/api/identity';
import { server } from '@/test/msw/node';
import { render } from '@/test/render';

const APPROVE = `/cli/approve?ticket=${mockCliTicket}`;

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

describe('CLI approval', () => {
  it('shows which account the CLI will act as and approves by keyboard', async () => {
    const user = userEvent.setup();
    signIn();
    render({ route: APPROVE });

    expect(
      await screen.findByRole('heading', {
        name: 'Sign in to the BackendRelay CLI?',
      }),
    ).toBeInTheDocument();
    expect(screen.getByText(mockSignInUser.email)).toBeInTheDocument();

    await user.tab();
    expect(
      screen.getByRole('link', { name: 'BackendRelay home' }),
    ).toHaveFocus();
    await user.tab();
    expect(screen.getByRole('button', { name: 'Deny' })).toHaveFocus();
    await user.tab();
    expect(screen.getByRole('button', { name: 'Approve' })).toHaveFocus();
    await user.keyboard('{Enter}');

    expect(
      await screen.findByRole('heading', { name: 'CLI sign-in approved' }),
    ).toBeInTheDocument();
    expect(
      screen.getByText('You can close this tab and return to your terminal.'),
    ).toBeInTheDocument();
    expect(readStoredSession(window.localStorage)?.token).toBe(mockSignInToken);
  });

  it('denies the ticket and ends the browser flow', async () => {
    const user = userEvent.setup();
    signIn();
    render({ route: APPROVE });

    await user.click(await screen.findByRole('button', { name: 'Deny' }));

    expect(
      await screen.findByRole('heading', { name: 'CLI sign-in denied' }),
    ).toBeInTheDocument();
    expect(
      screen.getByText('You can close this tab and return to your terminal.'),
    ).toBeInTheDocument();
  });

  it('sends a signed-out visitor to sign in and back here', async () => {
    const { router } = render({ route: APPROVE });

    await waitFor(() => {
      expect(router.state.location.pathname).toBe('/login');
    });
    expect(router.state.location.search).toMatchObject({
      redirect: APPROVE,
    });
    expect(
      await screen.findByRole('heading', { name: 'Sign in to BackendRelay' }),
    ).toBeInTheDocument();
  });

  it('rejects a link without a ticket', async () => {
    signIn();
    render({ route: '/cli/approve' });

    expect(
      await screen.findByRole('heading', {
        name: "This CLI sign-in link isn't valid",
      }),
    ).toBeInTheDocument();
    expect(screen.getByRole('alert')).toHaveTextContent(
      'The link is missing its ticket.',
    );
  });

  it('keeps the buttons usable and reports a failed claim', async () => {
    server.use(
      http.post('/dash/cli/auth/claim', () =>
        HttpResponse.json(
          { type: 'record-expired', message: 'This request expired.' },
          { status: 400 },
        ),
      ),
    );

    const user = userEvent.setup();
    signIn();
    render({ route: APPROVE });

    await user.click(await screen.findByRole('button', { name: 'Approve' }));

    expect(await screen.findByRole('alert')).toHaveTextContent(
      'This request expired.',
    );
    expect(screen.getByRole('button', { name: 'Approve' })).toBeEnabled();
  });
});
