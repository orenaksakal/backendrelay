import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { resetSession, setSession } from '@/lib/auth/session';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import { mockSignInToken, mockSignInUser } from '@/test/fixtures/api/auth';
import { render } from '@/test/render';

const ROUTE = '/operator/restores';

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

async function renderRestores() {
  const result = render({ route: ROUTE });
  await screen.findByRole('heading', { level: 1, name: 'Restores' });
  return result;
}

describe('operator restores', () => {
  it('lists the jobs with their app, status and progress', async () => {
    await renderRestores();

    const table = await screen.findByRole('table', { name: 'Restore jobs' });
    expect(within(table).getByText('headshot-ai copy')).toBeInTheDocument();
    expect(within(table).getByText('todo-app restore')).toBeInTheDocument();
    expect(within(table).getByText('analytics restore')).toBeInTheDocument();
    expect(within(table).getByText('Restore c9d4e5f6')).toBeInTheDocument();

    expect(within(table).getByText('Running')).toBeInTheDocument();
    expect(within(table).getByText('Waiting')).toBeInTheDocument();
    expect(within(table).getByText('Done')).toBeInTheDocument();
    expect(within(table).getByText('Failed')).toBeInTheDocument();
    expect(within(table).getByText('Cancelled')).toBeInTheDocument();
    expect(
      within(table).getByText('1,200 / 4,800 entities'),
    ).toBeInTheDocument();
    expect(
      (await within(table).findAllByText('headshot-ai')).length,
    ).toBeGreaterThan(0);
  });

  it('opens the detail dialog with the worker report', async () => {
    const user = userEvent.setup();
    await renderRestores();

    await user.click(
      await screen.findByRole('button', {
        name: 'View restore job headshot-ai copy',
      }),
    );

    const dialog = await screen.findByRole('dialog');
    expect(
      within(dialog).getByRole('heading', { name: 'headshot-ai copy' }),
    ).toBeInTheDocument();
    expect(within(dialog).getByText('Running')).toBeInTheDocument();
    expect(
      within(dialog).getByText('1,200 / 4,800 entities'),
    ).toBeInTheDocument();

    await user.click(within(dialog).getByRole('button', { name: 'Close' }));
    await waitFor(() => {
      expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    });
  });

  it('cancels a waiting job after confirmation', async () => {
    const user = userEvent.setup();
    await renderRestores();

    await user.click(
      await screen.findByRole('button', {
        name: 'Cancel restore job todo-app restore',
      }),
    );

    const confirm = await screen.findByRole('dialog');
    expect(
      within(confirm).getByText(/Cancel restore of todo-app restore/),
    ).toBeInTheDocument();

    await user.click(
      within(confirm).getByRole('button', { name: 'Cancel restore job' }),
    );

    expect(await screen.findByText('Restore cancelled')).toBeInTheDocument();
    const table = screen.getByRole('table', { name: 'Restore jobs' });
    const row = within(table).getByText('todo-app restore').closest('tr');
    expect(row).not.toBeNull();
    if (row === null) return;
    await waitFor(() => {
      expect(within(row).getByText('Cancelled')).toBeInTheDocument();
    });
  });
});
