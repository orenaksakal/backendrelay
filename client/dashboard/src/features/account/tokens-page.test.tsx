import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { resetSession, setSession } from '@/lib/auth/session';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import {
  mockCreatedPersonalToken,
  mockPersonalTokens,
} from '@/test/fixtures/api/account';
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

describe('account tokens', () => {
  it('lists tokens and shows a new token once', async () => {
    const user = userEvent.setup();
    render({ route: '/account/tokens' });

    expect(await screen.findByText('Platform Token')).toBeInTheDocument();
    expect(screen.getByText('9c1d')).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Create token' }));

    const createDialog = await screen.findByRole('dialog');
    const name = within(createDialog).getByLabelText('Name');
    await user.type(name, 'CI deploy');
    await user.click(
      within(createDialog).getByRole('button', { name: 'Create token' }),
    );

    expect(
      await screen.findByRole('heading', { name: 'Copy your token' }),
    ).toBeInTheDocument();
    expect(
      screen.getByText(mockCreatedPersonalToken.token),
    ).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: "I've saved it" }));

    expect(await screen.findByText('CI deploy')).toBeInTheDocument();
    expect(screen.getByText('4e2f')).toBeInTheDocument();
  });

  it('deletes a token only after confirmation', async () => {
    const user = userEvent.setup();
    render({ route: '/account/tokens' });

    const row = (
      await screen.findByText(mockPersonalTokens[0]?.name ?? '')
    ).closest('tr');
    expect(row).not.toBeNull();
    if (row === null) return;

    await user.click(within(row).getByRole('button', { name: 'Delete' }));
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByText('Platform Token')).toBeInTheDocument();

    await user.click(
      within(dialog).getByRole('button', { name: 'Delete token' }),
    );

    await waitFor(() => {
      expect(screen.queryByText('Platform Token')).not.toBeInTheDocument();
    });
    expect(
      await screen.findByText('No personal access tokens'),
    ).toBeInTheDocument();
  });
});
