import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { resetSession, setSession } from '@/lib/auth/session';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import type { AppInstantClient, AppSchemaState } from '@/lib/instant';
import { mockSignInToken, mockSignInUser } from '@/test/fixtures/api/auth';
import { mockDash } from '@/test/msw/handlers';
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
  const listeners = new Set<(result: unknown) => void>();

  const client = {
    core: {
      shutdown: () => undefined,
      subscribeQuery: (
        query: Record<string, unknown>,
        callback: (result: unknown) => void,
      ) => {
        queries.push(query);
        listeners.add(callback);
        const deliver = () => {
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
            return;
          }

          const namespace = namespaces[0] ?? '';
          callback({
            data: { [namespace]: rows[namespace] ?? [] },
            pageInfo: { [namespace]: { hasNextPage: false } },
          });
        };
        deliver();
        return () => {
          listeners.delete(callback);
        };
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

  const goals = {
    name: 'goals',
    isSystem: false,
    links: [],
    attributes: [
      {
        id: 'attr-goal-id',
        namespace: 'goals',
        name: 'id',
        type: 'string' as const,
        isIndexed: false,
        isUnique: true,
        isRequired: false,
        isPrimary: true,
      },
    ],
  };

  const users = {
    name: '$users',
    isSystem: true,
    links: [],
    attributes: [],
  };

  const schema: AppSchemaState = {
    namespaces: [todos, goals, users],
    attributes: [...todos.attributes, ...goals.attributes],
    links: [],
    attrs: {},
    isLoading: false,
  };

  return { client, schema, queries };
});

vi.mock('@instantdb/react', () => ({ init: () => ({}) }));

vi.mock('@/components/code-editor/code-editor', () => ({
  CodeEditor: () => null,
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
  setSession({ token: mockSignInToken, user: mockSignInUser });
});

afterEach(() => {
  resetSession();
  resetRuntimeConfig();
  window.localStorage.clear();
  window.sessionStorage.clear();
});

async function renderExplorer(
  route = '/apps/app-1/explorer',
  {
    heading = true,
    namespace = true,
  }: { heading?: boolean; namespace?: boolean } = {},
) {
  const result = render({ route });
  if (heading) {
    await waitFor(() => {
      expect(
        screen.getByRole('heading', { level: 1, name: 'Explorer' }),
      ).toBeInTheDocument();
    });
  }
  // The first namespace lands in the URL after the schema loads; wait for that
  // so a typing test does not race the page's first re-render.
  if (namespace) {
    await waitFor(() => {
      expect(result.router.state.location.search.ns).toBeDefined();
    });
  }
  return result;
}

describe('explorer read path', () => {
  it.each(['create', 'update', 'delete'] as const)(
    'only warns about public $users %s when it is not registration',
    async (action) => {
      const user = userEvent.setup();
      server.use(
        http.get('http://localhost:8888/dash', () =>
          HttpResponse.json({
            ...mockDash,
            apps: mockDash.apps.map((app) =>
              app.id === 'app-1'
                ? {
                    ...app,
                    rules: { $users: { allow: { [action]: 'true' } } },
                  }
                : app,
            ),
          }),
        ),
      );
      await renderExplorer();
      const rail = screen.getByRole('complementary', { name: 'Namespaces' });
      await user.click(within(rail).getByRole('button', { name: 'System' }));
      const users = within(rail).getByRole('button', { name: /^\$users/ });
      const warning = within(users).queryByRole('img', {
        name: 'Anyone can write to this namespace',
      });
      if (action === 'create') {
        expect(warning).not.toBeInTheDocument();
      } else {
        expect(warning).toBeInTheDocument();
      }
    },
  );

  it('selects the first user namespace and renders counts and rows', async () => {
    const { router } = await renderExplorer();

    await waitFor(() => {
      expect(router.state.location.search).toMatchObject({ ns: 'todos' });
    });

    const rail = await screen.findByRole('complementary', {
      name: 'Namespaces',
    });
    expect(
      within(rail).getByRole('button', { name: /^todos/ }),
    ).toBeInTheDocument();
    expect(within(rail).getByText('1,204')).toBeInTheDocument();
    expect(
      within(rail).getByRole('img', {
        name: 'Anyone can write to this namespace',
      }),
    ).toBeInTheDocument();

    const grid = await screen.findByTestId('data-grid');
    expect(within(grid).getByText('2 rows')).toBeInTheDocument();
    expect(within(grid).getByText('t1')).toBeInTheDocument();
    expect(within(grid).getByText('title')).toBeInTheDocument();
    expect(within(grid).getByText('done')).toBeInTheDocument();

    await waitFor(() => {
      expect(
        mocks.queries.some((query) => {
          const form = query['todos'];
          return (
            typeof form === 'object' &&
            form !== null &&
            '$' in form &&
            (form as { $: { limit?: number } }).$.limit === 100
          );
        }),
      ).toBe(true);
    });
  });

  it('opens the labeled columns menu and hides a column', async () => {
    const user = userEvent.setup();
    await renderExplorer();

    await user.click(screen.getByRole('button', { name: 'Columns' }));
    const group = await screen.findByRole('group', { name: 'Columns' });
    const done = within(group).getByRole('menuitemcheckbox', { name: 'done' });
    expect(done).toBeChecked();
    await user.click(done);

    expect(
      within(screen.getByTestId('data-grid')).queryByText('done'),
    ).not.toBeInTheDocument();
  });

  it('turns a quick filter into a URL where clause and a filter row', async () => {
    const user = userEvent.setup();
    const { router } = await renderExplorer();

    const search = await screen.findByRole('searchbox', {
      name: 'Search or filter',
    });
    await user.type(search, 'done:false');

    await waitFor(() => {
      const where = router.state.location.search.where;
      expect(where).toBeDefined();
      expect(JSON.parse(where ?? '{}')).toEqual({ done: false });
    });
    expect(screen.getByRole('button', { name: /Filter/ })).toHaveTextContent(
      '1',
    );
    expect(search).toHaveValue('');
  });

  it('serialises plain text as an OR of $ilike clauses', async () => {
    const user = userEvent.setup();
    const { router } = await renderExplorer();

    const search = await screen.findByRole('searchbox', {
      name: 'Search or filter',
    });
    await user.type(search, 'milk');

    await waitFor(() => {
      const where = router.state.location.search.where;
      expect(JSON.parse(where ?? '{}')).toEqual({
        or: [{ title: { $ilike: '%milk%' } }],
      });
    });
  });

  it('shows inline help for a quick filter it cannot parse', async () => {
    const user = userEvent.setup();
    await renderExplorer();

    const search = await screen.findByRole('searchbox', {
      name: 'Search or filter',
    });
    await user.type(search, 'missing:1');

    expect(await screen.findByRole('alert')).toHaveTextContent(
      'Unknown attribute "missing".',
    );
    expect(search).toHaveValue('missing:1');
  });

  it('reads sort and page size back out of the URL', async () => {
    await renderExplorer(
      '/apps/app-1/explorer?ns=todos&sort=title&dir=desc&size=200',
    );

    await waitFor(() => {
      expect(
        mocks.queries.some((query) => {
          const form = query['todos'];
          if (typeof form !== 'object' || form === null) return false;
          const options = (form as { $?: { order?: unknown } }).$;
          return (
            options?.order !== undefined &&
            JSON.stringify(options.order) === '{"title":"desc"}'
          );
        }),
      ).toBe(true);
    });

    const refresh = await screen.findByRole('button', { name: 'Refresh' });
    expect(refresh).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Live' })).toHaveAttribute(
      'aria-pressed',
      'true',
    );
  });

  it('stops the table at admin access and explains the role', async () => {
    await renderExplorer('/apps/app-2/explorer', { namespace: false });

    expect(
      await screen.findByText('The explorer needs admin access to this app.'),
    ).toBeInTheDocument();
    expect(
      screen.getByText("You don't have access to explorer"),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole('complementary', { name: 'Namespaces' }),
    ).not.toBeInTheDocument();
  });

  it('opens the entity sheet from ?new=1 and validates required fields', async () => {
    const user = userEvent.setup();
    await renderExplorer('/apps/app-1/explorer?ns=todos&new=1', {
      heading: false,
    });

    expect(
      await screen.findByRole('dialog', { name: 'todos entity' }),
    ).toBeInTheDocument();
    const title = screen.getByRole('textbox', { name: 'title' });
    expect(title).toHaveValue('');

    await user.click(screen.getByRole('button', { name: 'Save' }));

    expect(screen.getByText('This field is required.')).toBeInTheDocument();
  });

  it('opens the new-namespace dialog from the sidebar', async () => {
    const user = userEvent.setup();
    await renderExplorer();

    const rail = await screen.findByRole('complementary', {
      name: 'Namespaces',
    });
    await user.click(
      within(rail).getByRole('button', { name: 'New namespace' }),
    );

    expect(
      await screen.findByRole('dialog', { name: 'New namespace' }),
    ).toBeInTheDocument();
  });
});
