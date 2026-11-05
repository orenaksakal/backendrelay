import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import {
  readStoredSession,
  resetSession,
  setSession,
} from '@/lib/auth/session';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import type { AppInstantClient, AppSchemaState } from '@/lib/instant';
import { mockSignInToken, mockSignInUser } from '@/test/fixtures/api/auth';
import { mockReadOnlyApp } from '@/test/fixtures/api/apps';
import { server } from '@/test/msw/node';
import { render } from '@/test/render';

const mocks = vi.hoisted(() => {
  function isRecord(value: unknown): value is Record<string, unknown> {
    return value !== null && typeof value === 'object' && !Array.isArray(value);
  }

  const rows: Record<string, unknown[]> = {
    todos: [
      { id: 't1', title: 'Buy milk', done: false },
      { id: 't2', title: 'Ship v2', done: true },
    ],
    goals: [{ id: 'g1', title: 'Ship it' }],
    $users: [],
  };
  const counts: Record<string, number> = {
    todos: 1204,
    goals: 87,
    $users: 3,
  };
  const queries: Record<string, unknown>[] = [];

  const client = {
    core: {
      shutdown: () => undefined,
      subscribeQuery: (
        query: Record<string, unknown>,
        callback: (result: unknown) => void,
      ) => {
        queries.push(query);
        const namespaces = Object.keys(query);
        const countsQuery = namespaces.every((namespace) => {
          const form = query[namespace];
          const options = isRecord(form) ? form['$'] : undefined;
          return isRecord(options) && options['aggregate'] === 'count';
        });
        if (countsQuery) {
          const aggregate: Record<string, { count: number }> = {};
          for (const namespace of namespaces) {
            aggregate[namespace] = { count: counts[namespace] ?? 0 };
          }
          callback({ aggregate });
          return () => undefined;
        }

        const namespace = namespaces[0] ?? '';
        callback({
          data: { [namespace]: rows[namespace] ?? [] },
          pageInfo: { [namespace]: { hasNextPage: false } },
        });
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

  const todos = {
    name: 'todos',
    isSystem: false,
    links: [],
    attributes: [
      {
        id: 'attr-id',
        namespace: 'todos',
        name: 'id',
        type: 'string' as const,
        isIndexed: false,
        isUnique: true,
        isRequired: false,
        isPrimary: true,
      },
      {
        id: 'attr-title',
        namespace: 'todos',
        name: 'title',
        type: 'string' as const,
        isIndexed: true,
        isUnique: false,
        isRequired: true,
        isPrimary: false,
      },
      {
        id: 'attr-done',
        namespace: 'todos',
        name: 'done',
        type: 'boolean' as const,
        isIndexed: true,
        isUnique: false,
        isRequired: false,
        isPrimary: false,
      },
    ],
  };

  const schema: AppSchemaState = {
    namespaces: [
      todos,
      { name: 'goals', isSystem: false, links: [], attributes: [] },
      { name: '$users', isSystem: true, links: [], attributes: [] },
    ],
    attributes: todos.attributes,
    links: [],
    attrs: {},
    isLoading: false,
  };

  return { client, schema, queries };
});

vi.mock('@instantdb/react', () => ({ init: () => ({}) }));

vi.mock('@/components/code-editor/code-editor', () => ({
  CodeEditor: ({
    value,
    onChange,
    'aria-label': ariaLabel,
  }: {
    value: string;
    onChange?: (value: string) => void;
    'aria-label': string;
  }) => (
    <textarea
      aria-label={ariaLabel}
      value={value}
      onChange={(event) => {
        onChange?.(event.target.value);
      }}
    />
  ),
}));

vi.mock(
  '@/components/data-grid/data-grid',
  () => import('@/test/stubs/data-grid'),
);

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

beforeEach(() => {
  resetSession();
  resetRuntimeConfig();
  window.localStorage.clear();
  window.sessionStorage.clear();
  mocks.queries.length = 0;
});

afterEach(() => {
  resetSession();
  resetRuntimeConfig();
  window.localStorage.clear();
  window.sessionStorage.clear();
});

function signIn(): void {
  setSession({ token: mockSignInToken, user: mockSignInUser });
}

function renderDevtool(route: string) {
  return Promise.resolve(render({ route }));
}

describe('devtool page', () => {
  it('signs in inline and stays on the devtool route', async () => {
    const user = userEvent.setup();
    const { router } = await renderDevtool('/_devtool?appId=app-1');

    expect(
      await screen.findByRole('heading', { name: 'Sign in to BackendRelay' }),
    ).toBeInTheDocument();
    expect(screen.queryByRole('tab', { name: 'Explorer' })).toBeNull();

    await user.type(screen.getByLabelText('Email'), 'owner@example.com');
    await user.click(screen.getByRole('button', { name: 'Send code' }));
    await screen.findByText(/We sent a 6-digit code to/);
    await user.type(screen.getByLabelText('Code'), '123456');

    await waitFor(() => {
      expect(readStoredSession(window.localStorage)?.token).toBe(
        mockSignInToken,
      );
    });
    await waitFor(() => {
      expect(router.state.location.pathname).toBe('/_devtool');
    });
    expect(
      await screen.findByRole('tab', { name: 'Explorer' }),
    ).toBeInTheDocument();
  });

  it('shows the app title, the compact explorer and the help links', async () => {
    signIn();
    const user = userEvent.setup();
    await renderDevtool('/_devtool?appId=app-1');

    expect(await screen.findByText('todo-app')).toBeInTheDocument();
    expect(screen.getByRole('tab', { name: 'Explorer' })).toHaveAttribute(
      'aria-selected',
      'true',
    );

    const namespace = await screen.findByRole('combobox', {
      name: 'Namespace',
    });
    expect(namespace).toHaveTextContent('todos');
    const grid = await screen.findByTestId('data-grid');
    expect(within(grid).getByText('2 rows')).toBeInTheDocument();
    expect(within(grid).getByText('t1')).toBeInTheDocument();

    await user.click(screen.getByRole('tab', { name: 'Help' }));
    expect(
      screen.getByRole('link', { name: 'todo-app overview' }),
    ).toHaveAttribute('href', '/apps/app-1/overview');
    expect(screen.getByRole('link', { name: 'Explorer' })).toHaveAttribute(
      'href',
      '/apps/app-1/explorer',
    );
  });

  it('runs a query and shows the result as a table', async () => {
    signIn();
    const user = userEvent.setup();
    await renderDevtool('/_devtool?appId=app-1');

    await user.click(await screen.findByRole('tab', { name: 'Playground' }));
    const editor = await screen.findByLabelText('InstaQL query');
    await waitFor(() => {
      expect(editor).toHaveValue('{\n  "todos": {}\n}');
    });

    await user.click(screen.getByRole('button', { name: 'Run' }));
    expect(await screen.findByText(/2 entities/)).toBeInTheDocument();

    await user.click(screen.getByRole('tab', { name: 'Table' }));
    const table = await screen.findByRole('table', {
      name: 'todos results',
    });
    expect(within(table).getByText('Buy milk')).toBeInTheDocument();
    expect(mocks.queries.some((query) => 'todos' in query)).toBe(true);
  });

  it('reports an invalid query without running it', async () => {
    signIn();
    const user = userEvent.setup();
    await renderDevtool('/_devtool?appId=app-1');

    await user.click(await screen.findByRole('tab', { name: 'Playground' }));
    const editor = await screen.findByLabelText('InstaQL query');
    await user.clear(editor);
    await user.type(editor, '{{');
    await user.click(screen.getByRole('button', { name: 'Run' }));

    expect(await screen.findByRole('alert')).toHaveTextContent(
      'The query must be valid JSON.',
    );
    expect(screen.queryByRole('status')).toBeNull();
  });

  it('explains a missing app id', async () => {
    signIn();
    await renderDevtool('/_devtool');

    expect(
      await screen.findByRole('heading', { name: 'No app ID provided' }),
    ).toBeInTheDocument();
    expect(screen.queryByRole('tab')).toBeNull();
  });

  it('shows the app 404 and can sign out', async () => {
    signIn();
    server.use(
      http.get('http://localhost:8888/dash/apps/app-9', () =>
        HttpResponse.json({ message: 'Not found' }, { status: 404 }),
      ),
    );

    await renderDevtool('/_devtool?appId=app-9');

    expect(
      await screen.findByRole('heading', {
        name: "We couldn't find your app",
      }),
    ).toBeInTheDocument();
    expect(screen.getByText('app-9')).toBeInTheDocument();

    const user = userEvent.setup();
    await user.click(screen.getByRole('button', { name: 'Sign out' }));
    expect(readStoredSession(window.localStorage)).toBeNull();
  });

  it('gates the data tabs below admin access', async () => {
    signIn();
    server.use(
      http.get('http://localhost:8888/dash/apps/app-2', () =>
        HttpResponse.json({ app: mockReadOnlyApp }),
      ),
    );

    await renderDevtool('/_devtool?appId=app-2');

    expect(
      await screen.findByText("You don't have access to explorer"),
    ).toBeInTheDocument();
    expect(
      screen.getByText('The explorer needs admin access to this app.'),
    ).toBeInTheDocument();
  });

  it('closes the iframe through the SDK message protocol', async () => {
    signIn();
    const user = userEvent.setup();
    const postMessage = vi.spyOn(window, 'postMessage');
    await renderDevtool('/_devtool?appId=app-1');

    await screen.findByRole('tab', { name: 'Explorer' });
    await user.click(screen.getByRole('button', { name: 'Close devtool' }));

    expect(postMessage).toHaveBeenCalledWith({ type: 'close' }, '*');
    postMessage.mockRestore();
  });
});
