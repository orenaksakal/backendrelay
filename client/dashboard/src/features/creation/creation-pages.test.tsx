import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { resetSession, setSession } from '@/lib/auth/session';
import { isUuid } from '@/lib/creation/creation';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import { mockSignInToken, mockSignInUser } from '@/test/fixtures/api/auth';
import { mockDash } from '@/test/msw/handlers';
import { server } from '@/test/msw/node';
import { render } from '@/test/render';

const API = 'http://localhost:8888';
const ORG_ID = '11111111-1111-4111-8111-111111111111';

interface PostedApp {
  id: string;
  admin_token: string;
  title: string;
  org_id?: string;
}

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

describe('create app', () => {
  it('creates an app with generated credentials and opens it', async () => {
    const user = userEvent.setup();
    let posted: PostedApp | undefined;
    server.use(
      http.post(`${API}/dash/apps`, async ({ request }) => {
        posted = (await request.json()) as PostedApp;
        return HttpResponse.json({
          app: {
            id: posted.id,
            title: posted.title,
            created_at: '2026-01-01T00:00:00Z',
            status: 'active',
            role: 'owner',
            org_id: posted.org_id ?? null,
          },
        });
      }),
    );

    const { router } = render({ route: '/new' });
    await screen.findByRole('heading', { level: 1, name: 'Create app' });

    const name = screen.getByLabelText('App name');
    expect(name).toHaveFocus();
    await user.type(name, '  todo-app  ');
    await user.click(screen.getByRole('button', { name: 'Create app' }));

    await waitFor(() => {
      expect(posted).toBeDefined();
    });
    if (posted === undefined) return;
    const created = posted;

    expect(created.title).toBe('todo-app');
    expect(isUuid(created.id)).toBe(true);
    expect(isUuid(created.admin_token)).toBe(true);
    expect(created.org_id).toBeUndefined();

    await waitFor(() => {
      expect(router.state.location.pathname).toBe(
        `/apps/${created.id}/overview`,
      );
    });
    expect(await screen.findByText('App created')).toBeInTheDocument();
  });

  it('preselects the workspace from ?workspace=', async () => {
    const user = userEvent.setup();
    let posted: PostedApp | undefined;
    server.use(
      http.get(`${API}/dash`, () =>
        HttpResponse.json({
          ...mockDash,
          orgs: [
            {
              id: ORG_ID,
              title: 'Acme',
              created_at: '2026-01-01T00:00:00Z',
              role: 'owner',
              paid: false,
            },
          ],
        }),
      ),
      http.post(`${API}/dash/apps`, async ({ request }) => {
        posted = (await request.json()) as PostedApp;
        return HttpResponse.json({
          app: {
            id: posted.id,
            title: posted.title,
            created_at: '2026-01-01T00:00:00Z',
            status: 'active',
            role: 'owner',
            org_id: posted.org_id ?? null,
          },
        });
      }),
    );

    render({ route: `/new?workspace=${ORG_ID}` });
    expect(await screen.findByText('Acme')).toBeInTheDocument();

    await user.type(screen.getByLabelText('App name'), 'org-app');
    await user.click(screen.getByRole('button', { name: 'Create app' }));

    await waitFor(() => {
      expect(posted).toBeDefined();
    });
    expect(posted?.org_id).toBe(ORG_ID);
  });

  it('shows the app-name error without sending a request', async () => {
    const user = userEvent.setup();
    let requests = 0;
    server.use(
      http.post(`${API}/dash/apps`, () => {
        requests += 1;
        return HttpResponse.json({});
      }),
    );

    render({ route: '/new' });
    await screen.findByRole('heading', { level: 1, name: 'Create app' });

    await user.type(screen.getByLabelText('App name'), 'a'.repeat(81));
    await user.click(screen.getByRole('button', { name: 'Create app' }));

    expect(
      await screen.findByText('App names can contain up to 80 characters.'),
    ).toBeInTheDocument();
    expect(requests).toBe(0);
  });

  it('recovers a committed app when the create response is lost', async () => {
    const user = userEvent.setup();
    let createdId = '';
    server.use(
      http.post(`${API}/dash/apps`, async ({ request }) => {
        const body = (await request.json()) as PostedApp;
        createdId = body.id;
        return HttpResponse.error();
      }),
      http.get(`${API}/dash/apps/:appId`, ({ params }) =>
        HttpResponse.json({
          app: {
            id: String(params['appId']),
            title: 'Recovered app',
            created_at: '2026-01-01T00:00:00Z',
            status: 'active',
            role: 'owner',
          },
        }),
      ),
    );

    const { router } = render({ route: '/new' });
    await screen.findByRole('heading', { level: 1, name: 'Create app' });

    await user.type(screen.getByLabelText('App name'), 'recovered-app');
    await user.click(screen.getByRole('button', { name: 'Create app' }));

    await waitFor(() => {
      expect(createdId).not.toBe('');
      expect(router.state.location.pathname).toBe(
        `/apps/${createdId}/overview`,
      );
    });
  });

  it('shows the original network error when the lookup fails', async () => {
    const user = userEvent.setup();
    server.use(
      http.post(`${API}/dash/apps`, () => HttpResponse.error()),
      http.get(`${API}/dash/apps/:appId`, () =>
        HttpResponse.json(
          { type: 'record-not-found', message: 'App not found' },
          { status: 404 },
        ),
      ),
    );

    render({ route: '/new' });
    await screen.findByRole('heading', { level: 1, name: 'Create app' });

    await user.type(screen.getByLabelText('App name'), 'unreachable-app');
    await user.click(screen.getByRole('button', { name: 'Create app' }));

    expect(
      (
        await screen.findAllByText(
          'Could not reach the server. Check your connection.',
        )
      ).length,
    ).toBeGreaterThan(0);
  });
});

describe('create org', () => {
  it('creates an org from the dialog and closes it', async () => {
    const user = userEvent.setup();
    let posted: { title?: string } | undefined;
    server.use(
      http.post(`${API}/dash/orgs`, async ({ request }) => {
        posted = (await request.json()) as { title?: string };
        return HttpResponse.json({
          org: {
            id: ORG_ID,
            title: posted.title,
            created_at: '2026-01-01T00:00:00Z',
            updated_at: '2026-01-01T00:00:00Z',
          },
        });
      }),
    );

    render({ route: '/new' });
    await screen.findByRole('heading', { level: 1, name: 'Create app' });

    await user.click(
      screen.getByRole('button', { name: 'Create an org instead' }),
    );
    const dialog = await screen.findByRole('dialog');
    await user.type(within(dialog).getByLabelText('Org name'), 'Acme');
    await user.click(
      within(dialog).getByRole('button', { name: 'Create org' }),
    );

    await waitFor(() => {
      expect(
        screen.queryByRole('dialog', { name: 'Create an org' }),
      ).not.toBeInTheDocument();
    });
    expect(posted?.title).toBe('Acme');
    expect(await screen.findByText('Org created')).toBeInTheDocument();
  });
});

describe('welcome', () => {
  it('shows the first-run heading without the org link', async () => {
    render({ route: '/welcome' });

    expect(
      await screen.findByRole('heading', {
        level: 1,
        name: 'Create your first app',
      }),
    ).toBeInTheDocument();
    expect(screen.getByLabelText('App name')).toBeInTheDocument();
    expect(
      screen.queryByRole('button', { name: 'Create an org instead' }),
    ).not.toBeInTheDocument();
  });
});
