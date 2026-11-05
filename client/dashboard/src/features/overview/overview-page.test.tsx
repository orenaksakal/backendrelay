import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { resetSession, setSession } from '@/lib/auth/session';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import { mockSignInToken, mockSignInUser } from '@/test/fixtures/api/auth';
import { mockAppUsage } from '@/test/fixtures/api/apps';
import { server } from '@/test/msw/node';
import { render } from '@/test/render';

const API = 'http://localhost:8888';
const ROUTE = '/apps/app-1/overview';

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

describe('app overview', () => {
  it('shows the app header, KPI values, health checks and session origins', async () => {
    render({ route: ROUTE });

    expect(
      await screen.findByRole('heading', { level: 1, name: 'todo-app' }),
    ).toBeInTheDocument();
    expect(screen.getByText('Active')).toBeInTheDocument();
    expect(screen.getByText(/Created/)).toBeInTheDocument();

    expect(await screen.findByText('87')).toBeInTheDocument();
    expect(await screen.findByText('4.2 MB')).toBeInTheDocument();
    expect(await screen.findByText('120 MB')).toBeInTheDocument();
    expect(await screen.findByText('240 files')).toBeInTheDocument();

    expect(
      await screen.findByText('Permissions: 1 namespace allows public writes'),
    ).toBeInTheDocument();
    expect(
      await screen.findByText(/Backups: last backup .* no schedule/),
    ).toBeInTheDocument();
    expect(await screen.findByText('Webhooks: 1 disabled')).toBeInTheDocument();
    expect(
      await screen.findByText(
        'Auth: custom sender domain not verified (using the default BackendRelay sender)',
      ),
    ).toBeInTheDocument();

    const origins = await screen.findByRole('table', {
      name: 'Live session origins',
    });
    expect(
      within(origins).getByText('https://app.example.com'),
    ).toBeInTheDocument();
    expect(
      within(origins).getByText('https://admin.example.com'),
    ).toBeInTheDocument();
    expect(within(origins).getByText('8')).toBeInTheDocument();

    expect(
      screen.getByRole('link', { name: 'Open permissions' }),
    ).toHaveAttribute('href', '/apps/app-1/permissions?view=overview');
  });

  it('opens the Connect sheet with real IDs, URLs and CLI environment', async () => {
    const user = userEvent.setup();
    render({ route: ROUTE });
    await screen.findByRole('heading', { level: 1, name: 'todo-app' });

    await user.click(screen.getByRole('button', { name: 'Connect' }));
    const sheet = await screen.findByRole('dialog');

    expect(
      within(sheet).getByRole('heading', { name: 'Connect to todo-app' }),
    ).toBeInTheDocument();
    expect(
      within(sheet).getByText('http://localhost:8888'),
    ).toBeInTheDocument();
    expect(
      within(sheet).getByText('ws://localhost:8888/runtime/session'),
    ).toBeInTheDocument();
    expect(
      within(sheet).getByRole('button', { name: 'Copy app ID' }),
    ).toBeInTheDocument();

    await user.click(within(sheet).getByRole('tab', { name: 'CLI' }));
    expect(
      await within(sheet).findByText(
        /INSTANT_CLI_API_URI=http:\/\/localhost:8888/,
      ),
    ).toBeInTheDocument();
    expect(
      within(sheet).getByText(/npx @backendrelay\/cli init --app app-1/),
    ).toBeInTheDocument();

    await user.click(within(sheet).getByRole('button', { name: 'Done' }));
    await waitFor(() => {
      expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    });
  });

  it('shows KPI card skeletons while usage loads, then the values', async () => {
    let release: () => void = () => undefined;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    server.use(
      http.get(`${API}/dash/apps/app-1/usage`, async () => {
        await gate;
        return HttpResponse.json(mockAppUsage);
      }),
    );

    render({ route: ROUTE });
    await screen.findByRole('heading', { level: 1, name: 'todo-app' });

    await waitFor(() => {
      expect(
        document.querySelectorAll('[data-slot="card-skeleton"]'),
      ).toHaveLength(4);
    });

    release();
    expect(await screen.findByText('4.2 MB')).toBeInTheDocument();
    expect(
      document.querySelectorAll('[data-slot="card-skeleton"]'),
    ).toHaveLength(0);
  });
});
