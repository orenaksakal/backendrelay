import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { resetSession, setSession } from '@/lib/auth/session';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import { THEME_STORAGE_KEY } from '@/lib/theme';
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

describe('top bar menus', () => {
  it('opens the account menu and navigates to the profile', async () => {
    const user = userEvent.setup();
    const { router } = render({ route: '/apps/app-1/overview' });

    await user.click(
      await screen.findByRole('button', { name: 'Account menu' }),
    );
    const group = await screen.findByRole('group', {
      name: mockSignInUser.email,
    });
    await user.click(within(group).getByRole('menuitem', { name: 'Account' }));

    expect(
      await screen.findByRole('heading', { level: 1, name: 'Profile' }),
    ).toBeInTheDocument();
    expect(router.state.location.pathname).toBe('/account/profile');
    expect(screen.queryByText('Something went wrong!')).not.toBeInTheDocument();
  });

  it('opens the theme menu and persists the selected theme', async () => {
    const user = userEvent.setup();
    render({ route: '/account/profile' });

    await user.click(
      await screen.findByRole('button', { name: 'Theme: System' }),
    );
    const group = await screen.findByRole('group', { name: 'Theme' });
    await user.click(
      within(group).getByRole('menuitemradio', { name: 'Dark' }),
    );

    await waitFor(() => {
      expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBe('dark');
    });
    expect(
      screen.getByRole('button', { name: 'Theme: Dark' }),
    ).toBeInTheDocument();
  });
});
