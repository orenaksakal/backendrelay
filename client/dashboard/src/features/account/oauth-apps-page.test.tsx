import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { resetSession, setSession } from '@/lib/auth/session';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import { mockAuthorizedApps } from '@/test/fixtures/api/account';
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

describe('account OAuth apps', () => {
  it('lists scopes and revokes access after confirmation', async () => {
    const user = userEvent.setup();
    render({ route: '/account/oauth-apps' });

    expect(await screen.findByText('Linear sync')).toBeInTheDocument();
    expect(screen.getAllByText('apps-read').length).toBeGreaterThan(0);
    expect(screen.getByText('data-read')).toBeInTheDocument();

    const first = mockAuthorizedApps[0];
    const row = screen.getByText(first?.name ?? '').closest('tr');
    expect(row).not.toBeNull();
    if (row === null) return;

    await user.click(
      within(row).getByRole('button', { name: 'Revoke access' }),
    );
    const dialog = await screen.findByRole('dialog');
    await user.click(
      within(dialog).getByRole('button', { name: 'Revoke access' }),
    );

    await waitFor(() => {
      expect(screen.queryByText('Linear sync')).not.toBeInTheDocument();
    });
    expect(screen.getByText('Release notes')).toBeInTheDocument();
  });
});
