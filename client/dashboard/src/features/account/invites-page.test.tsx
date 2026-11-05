import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { resetSession, setSession } from '@/lib/auth/session';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import { mockSignInToken, mockSignInUser } from '@/test/fixtures/api/auth';
import { render } from '@/test/render';

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

describe('account invites', () => {
  it('declines an invite and removes it from the list', async () => {
    const user = userEvent.setup();
    render({ route: '/account/invites' });

    const table = await screen.findByRole('table', { name: 'Pending invites' });
    const row = within(table).getByText('Acme').closest('tr');
    expect(row).not.toBeNull();
    if (row === null) return;

    await user.click(within(row).getByRole('button', { name: 'Decline' }));

    await waitFor(() => {
      expect(within(table).queryByText('Acme')).not.toBeInTheDocument();
    });
    expect(within(table).getByText('todo-app')).toBeInTheDocument();
  });

  it('accepts an app invite and opens the app overview', async () => {
    const user = userEvent.setup();
    const { router } = render({ route: '/account/invites' });

    const table = await screen.findByRole('table', { name: 'Pending invites' });
    const row = within(table).getByText('todo-app').closest('tr');
    expect(row).not.toBeNull();
    if (row === null) return;

    await user.click(within(row).getByRole('button', { name: 'Accept' }));

    await waitFor(() => {
      expect(router.state.location.pathname).toBe('/apps/app-1/overview');
    });
    expect(
      await screen.findByRole('heading', { level: 1, name: 'todo-app' }),
    ).toBeInTheDocument();
  });
});
