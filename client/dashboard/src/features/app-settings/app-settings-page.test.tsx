import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { resetSession, setSession } from '@/lib/auth/session';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import { mockSignInToken, mockSignInUser } from '@/test/fixtures/api/auth';
import { mockPersonalApps, mockTodoApp } from '@/test/fixtures/api/apps';
import { mockDash } from '@/test/msw/handlers';
import { server } from '@/test/msw/node';
import { render } from '@/test/render';

const API = 'http://localhost:8888';
const ROUTE = '/apps/app-1/settings';

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

describe('app settings', () => {
  it('shows the app details and pauses writes', async () => {
    const user = userEvent.setup();
    let status: unknown;
    server.use(
      http.post(`${API}/dash/apps/app-1/status`, async ({ request }) => {
        status = await request.json();
        return HttpResponse.json({ status: 'read-only' });
      }),
    );

    render({ route: ROUTE });

    expect(
      await screen.findByRole('heading', { level: 1, name: 'Settings' }),
    ).toBeInTheDocument();
    expect(await screen.findByLabelText('Name')).toHaveValue('todo-app');
    expect(
      screen.getByRole('button', { name: 'Copy ID app-1' }),
    ).toBeInTheDocument();
    expect(screen.getByText('Workspace')).toBeInTheDocument();

    const pause = screen.getByRole('switch', { name: 'Pause writes' });
    expect(pause).not.toBeChecked();
    await user.click(pause);

    await waitFor(() => {
      expect(status).toEqual({ status: 'read-only' });
    });
    expect(await screen.findAllByText('Read-only')).not.toHaveLength(0);
  });

  it('renames the app from the unsaved-changes bar', async () => {
    const user = userEvent.setup();
    let renamed: unknown;
    server.use(
      http.post(`${API}/dash/apps/app-1/rename`, async ({ request }) => {
        renamed = await request.json();
        return HttpResponse.json({});
      }),
    );

    render({ route: ROUTE });
    const name = await screen.findByLabelText('Name');
    await user.clear(name);
    await user.type(name, 'todo-app v2');

    expect(screen.getByText('You have unsaved changes')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => {
      expect(renamed).toEqual({ title: 'todo-app v2' });
    });
    expect(await screen.findByText('App renamed')).toBeInTheDocument();
    expect(screen.getByLabelText('Name')).toHaveValue('todo-app v2');
  });

  it('reveals and rotates the admin token, showing the new value once', async () => {
    const user = userEvent.setup();
    render({ route: `${ROUTE}?tab=token` });

    await screen.findByRole('heading', { level: 1, name: 'Settings' });
    const main = screen.getByRole('main');
    expect(await screen.findByText('app-1-••••••••')).toBeInTheDocument();

    await user.click(within(main).getByRole('button', { name: 'Show secret' }));
    expect(
      await screen.findByText('app-1-admin-token-0123456789'),
    ).toBeInTheDocument();

    await user.click(within(main).getByRole('button', { name: 'Rotate' }));
    const confirm = await screen.findByRole('dialog');
    await user.type(within(confirm).getByRole('textbox'), 'rotate');
    await user.click(within(confirm).getByRole('button', { name: 'Rotate' }));

    const result = await screen.findByRole('dialog');
    expect(
      within(result).getByText('Copy the new admin token'),
    ).toBeInTheDocument();
    expect(
      within(result).getByText('app-1-rotated-token-9876543210'),
    ).toBeInTheDocument();

    await user.click(
      within(result).getByRole('button', { name: "I've saved it" }),
    );
    await waitFor(() => {
      expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    });
  });

  it('lists app members and invites a teammate', async () => {
    const user = userEvent.setup();
    let sent: unknown;
    server.use(
      http.post(`${API}/dash/apps/app-1/invite/send`, async ({ request }) => {
        sent = await request.json();
        return HttpResponse.json({});
      }),
    );

    render({ route: `${ROUTE}?tab=team` });
    await screen.findByRole('heading', { level: 1, name: 'Settings' });

    const members = await screen.findByRole('table', { name: 'App members' });
    expect(within(members).getByText('admin@example.com')).toBeInTheDocument();
    expect(within(members).getByText('Owner')).toBeInTheDocument();

    const invites = screen.getByRole('table', { name: 'Pending invites' });
    expect(within(invites).getByText('new@example.com')).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Invite member' }));
    const dialog = await screen.findByRole('dialog');
    await user.type(
      within(dialog).getByRole('textbox', { name: 'Email' }),
      'teammate@example.com',
    );
    await user.click(
      within(dialog).getByRole('button', { name: 'Send invite' }),
    );

    await waitFor(() => {
      expect(sent).toEqual({
        'invitee-email': 'teammate@example.com',
        role: 'collaborator',
      });
    });
    expect(await screen.findByText('Invite sent')).toBeInTheDocument();
  });

  it('deletes the app only after the typed confirmation', async () => {
    const user = userEvent.setup();
    let deleted = false;
    server.use(
      http.delete(`${API}/dash/apps/app-1`, () => {
        deleted = true;
        return HttpResponse.json({ ok: true });
      }),
    );

    const { router } = render({ route: `${ROUTE}?tab=danger` });
    await screen.findByRole('heading', { level: 1, name: 'Settings' });

    await user.click(screen.getByRole('button', { name: 'Delete app' }));
    const dialog = await screen.findByRole('dialog');
    const confirm = within(dialog).getByRole('button', { name: 'Delete app' });
    expect(confirm).toBeDisabled();

    await user.type(within(dialog).getByRole('textbox'), 'todo-app');
    expect(confirm).toBeEnabled();
    await user.click(confirm);

    await waitFor(() => {
      expect(deleted).toBe(true);
    });
    await waitFor(() => {
      expect(router.state.location.pathname).toBe('/w/personal');
    });
    expect(
      await screen.findByText('App scheduled for deletion'),
    ).toBeInTheDocument();
  });

  it('hides admin-only controls from a collaborator', async () => {
    server.use(
      http.get(`${API}/dash`, () =>
        HttpResponse.json({
          ...mockDash,
          apps: mockPersonalApps.map((app) =>
            app.id === mockTodoApp.id ? { ...app, role: 'collaborator' } : app,
          ),
        }),
      ),
      http.get(`${API}/dash/apps/:appId`, ({ params }) =>
        HttpResponse.json({
          app: {
            ...mockTodoApp,
            id: String(params['appId']),
            role: 'collaborator',
          },
        }),
      ),
    );

    render({ route: `${ROUTE}?tab=token` });

    expect(
      await screen.findByText("You don't have access to the admin token"),
    ).toBeInTheDocument();
  });
});
