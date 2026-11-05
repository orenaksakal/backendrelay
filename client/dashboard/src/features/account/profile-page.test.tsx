import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import {
  readStoredSession,
  resetSession,
  setSession,
} from '@/lib/auth/session';
import { PAGE_SIZE_STORAGE_KEY } from '@/lib/account/preferences';
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

describe('account profile', () => {
  it('redirects /account to the profile screen', async () => {
    const { router } = render({ route: '/account' });

    await waitFor(() => {
      expect(router.state.location.pathname).toBe('/account/profile');
    });
    expect(
      await screen.findByRole('heading', { level: 1, name: 'Profile' }),
    ).toBeInTheDocument();
  });

  it('shows the email and remembers the default page size', async () => {
    const user = userEvent.setup();
    render({ route: '/account/profile' });

    expect(await screen.findAllByText(mockSignInUser.email)).not.toHaveLength(
      0,
    );

    expect(
      screen.queryByRole('switch', {
        name: 'Require unlock before editing data',
      }),
    ).not.toBeInTheDocument();

    const pageSize = screen.getByRole('combobox', {
      name: 'Default page size',
    });
    expect(pageSize).toHaveTextContent('50');
    await user.click(pageSize);
    await user.click(await screen.findByRole('option', { name: '100' }));

    expect(pageSize).toHaveTextContent('100');
    expect(window.localStorage.getItem(PAGE_SIZE_STORAGE_KEY)).toBe('100');
  });

  it('signs out through the server and clears the session', async () => {
    const user = userEvent.setup();
    const { router } = render({ route: '/account/profile' });

    await user.click(await screen.findByRole('button', { name: 'Sign out' }));

    await waitFor(() => {
      expect(router.state.location.pathname).toBe('/login');
    });
    expect(readStoredSession(window.localStorage)).toBeNull();
  });
});
