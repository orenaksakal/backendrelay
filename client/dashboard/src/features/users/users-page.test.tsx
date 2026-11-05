import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { resetSession, setSession } from '@/lib/auth/session';
import type { AppInstantClient } from '@/lib/instant';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import { mockSignInToken, mockSignInUser } from '@/test/fixtures/api/auth';
import { mockDash } from '@/test/msw/handlers';
import { server } from '@/test/msw/node';
import { render } from '@/test/render';

const mocks = vi.hoisted(() => {
  const rows = [
    {
      id: 'user-aaa',
      email: 'ada@example.com',
      type: 'user',
      createdAt: '2026-10-20T12:00:00Z',
    },
    { id: 'guest-bbb', type: 'guest' },
  ];
  const queries: Record<string, unknown>[] = [];

  function resultFor(query: Record<string, unknown>): unknown {
    const root = query['$users'];
    const form =
      root !== null && typeof root === 'object'
        ? (root as Record<string, unknown>)
        : {};
    const options =
      form['$'] !== null && typeof form['$'] === 'object'
        ? (form['$'] as Record<string, unknown>)
        : {};
    const where =
      options['where'] !== null && typeof options['where'] === 'object'
        ? (options['where'] as Record<string, unknown>)
        : {};
    const users =
      typeof where['id'] === 'string'
        ? [{ ...(rows[0] ?? {}), todos: [{ id: 'todo-1', title: 'Buy milk' }] }]
        : rows;
    return {
      data: { $users: users },
      pageInfo: { $users: { hasNextPage: false } },
    };
  }

  const client = {
    core: {
      shutdown: () => undefined,
      subscribeQuery: (
        query: Record<string, unknown>,
        callback: (result: unknown) => void,
      ) => {
        queries.push(query);
        callback(resultFor(query));
        return () => undefined;
      },
      _reactor: {
        subscribeAttrs: () => () => undefined,
        pushOps: () => Promise.resolve(undefined),
      },
    },
    tx: {},
    transact: () => Promise.resolve(undefined),
  } satisfies AppInstantClient;

  const schema = {
    namespaces: [
      {
        name: '$users',
        isSystem: true,
        attributes: [],
        links: [
          {
            id: 'attr-owner',
            namespace: '$users',
            label: 'todos',
            targetNamespace: 'todos',
            reverseLabel: 'owner',
            cardinality: 'many' as const,
            onDelete: undefined,
          },
        ],
      },
    ],
    attributes: [],
    links: [],
    attrs: null,
    isLoading: false,
  };

  return {
    client,
    schema,
    queries,
    createUser: vi.fn(() => Promise.resolve(undefined)),
    sendMagicCode: vi.fn(() => Promise.resolve(undefined)),
    signOutUser: vi.fn(() => Promise.resolve(undefined)),
    deleteUser: vi.fn(() => Promise.resolve(undefined)),
  };
});

vi.mock('@instantdb/react', () => ({ init: () => ({}) }));

vi.mock('@/lib/instant', async () => {
  const actual =
    await vi.importActual<typeof import('@/lib/instant')>('@/lib/instant');
  return {
    ...actual,
    useAppClient: () => mocks.client,
    peekAppClient: () => mocks.client,
    useAppSchema: () => mocks.schema,
  };
});

vi.mock('@/lib/users/admin', async () => {
  const actual =
    await vi.importActual<typeof import('@/lib/users/admin')>(
      '@/lib/users/admin',
    );
  return {
    ...actual,
    createUser: mocks.createUser,
    sendMagicCode: mocks.sendMagicCode,
    signOutUser: mocks.signOutUser,
    deleteUser: mocks.deleteUser,
  };
});

beforeEach(() => {
  resetSession();
  resetRuntimeConfig();
  window.localStorage.clear();
  window.sessionStorage.clear();
  mocks.queries.length = 0;
  vi.clearAllMocks();
  setSession({ token: mockSignInToken, user: mockSignInUser });
});

afterEach(() => {
  resetSession();
  resetRuntimeConfig();
  window.localStorage.clear();
  window.sessionStorage.clear();
});

async function renderUsers(route = '/apps/app-1/users') {
  const result = render({ route });
  await waitFor(() => {
    const heading = screen.queryByRole('heading', { level: 1, name: 'Users' });
    const dialog = screen.queryByRole('dialog');
    expect(heading ?? dialog).not.toBeNull();
  });
  return result;
}

describe('users page', () => {
  it('lists users and opens the sheet with linked data', async () => {
    const user = userEvent.setup();
    const { router } = await renderUsers();

    const table = await screen.findByRole('table', { name: 'Users' });
    expect(within(table).getByText('ada@example.com')).toBeInTheDocument();
    expect(within(table).getByText('Guest')).toBeInTheDocument();
    expect(screen.getByText('2 users loaded')).toBeInTheDocument();

    await user.click(within(table).getByText('ada@example.com'));

    await waitFor(() => {
      expect(router.state.location.search).toMatchObject({
        user: 'user-aaa',
      });
    });
    expect(
      await screen.findByRole('heading', { name: 'ada@example.com' }),
    ).toBeInTheDocument();

    const sheet = screen.getByRole('dialog');
    await user.click(within(sheet).getByRole('tab', { name: 'Linked data' }));

    expect(within(sheet).getByText('Buy milk')).toBeInTheDocument();
    expect(
      within(sheet).getByRole('link', { name: 'Open in Explorer' }),
    ).toHaveAttribute(
      'href',
      expect.stringContaining('/apps/app-1/explorer?ns=todos'),
    );
  });

  it('filters guests through the URL', async () => {
    await renderUsers('/apps/app-1/users?type=guest');

    expect(
      screen.getByRole('combobox', { name: 'Filter by user type' }),
    ).toBeInTheDocument();
    expect(
      mocks.queries.some((query) => JSON.stringify(query).includes('$isNull')),
    ).toBe(true);
  });

  it('creates a user from the dialog without showing a token', async () => {
    const user = userEvent.setup();
    await renderUsers();

    await user.click(screen.getByRole('button', { name: 'Create user' }));
    const title = await screen.findByRole('heading', { name: 'Create user' });
    const dialog = title.closest('[role="dialog"]');
    expect(dialog).not.toBeNull();
    await user.type(
      within(dialog as HTMLElement).getByLabelText('Email'),
      'new@example.com',
    );
    await user.click(
      within(dialog as HTMLElement).getByRole('button', {
        name: 'Create user',
      }),
    );

    await waitFor(() => {
      expect(mocks.createUser).toHaveBeenCalledWith(
        expect.objectContaining({ appId: 'app-1' }),
        'new@example.com',
      );
    });
  });

  it('deletes a user after typing their email', async () => {
    const user = userEvent.setup();
    await renderUsers('/apps/app-1/users?user=user-aaa');

    const sheet = await screen.findByRole('dialog');
    await user.click(
      within(sheet).getByRole('button', { name: 'Delete user' }),
    );

    const confirmTitle = await screen.findByRole('heading', {
      name: 'Delete this user?',
    });
    const confirm = confirmTitle.closest('[role="dialog"]');
    expect(confirm).not.toBeNull();
    await user.type(
      within(confirm as HTMLElement).getByRole('textbox'),
      'ada@example.com',
    );
    await user.click(
      within(confirm as HTMLElement).getByRole('button', {
        name: 'Delete user',
      }),
    );

    await waitFor(() => {
      expect(mocks.deleteUser).toHaveBeenCalledWith(
        expect.objectContaining({ appId: 'app-1' }),
        'user-aaa',
      );
    });
  });

  it.each(['owner', 'admin'] as const)(
    'lets an %s create a user even with the obsolete lock preference',
    async (role) => {
      const user = userEvent.setup();
      window.localStorage.setItem('br.requireUnlock', 'true');
      server.use(
        http.get('http://localhost:8888/dash', () =>
          HttpResponse.json({
            ...mockDash,
            apps: mockDash.apps.map((app) =>
              app.id === 'app-1' ? { ...app, role } : app,
            ),
          }),
        ),
      );
      await renderUsers();

      await user.click(screen.getByRole('button', { name: 'Create user' }));
      const dialog = await screen.findByRole('dialog', { name: 'Create user' });
      await user.type(
        within(dialog).getByLabelText('Email'),
        'new@example.com',
      );
      await user.click(
        within(dialog).getByRole('button', { name: 'Create user' }),
      );

      await waitFor(() => {
        expect(mocks.createUser).toHaveBeenCalledWith(
          expect.objectContaining({ appId: 'app-1' }),
          'new@example.com',
        );
      });
      expect(screen.queryByText(/Editing is locked/)).not.toBeInTheDocument();
    },
  );

  it('stops below admin access and explains the role', async () => {
    await renderUsers('/apps/app-2/users');

    expect(
      await screen.findByText("You don't have access to users"),
    ).toBeInTheDocument();
    expect(
      screen.getByText('The user list needs admin access to this app.'),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole('table', { name: 'Users' }),
    ).not.toBeInTheDocument();
  });
});
