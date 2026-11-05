import { act, screen, waitFor, within } from '@testing-library/react';
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
  const client = {
    core: {
      shutdown: () => undefined,
      subscribeQuery: () => () => undefined,
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
    links: [
      {
        id: 'attr-owner',
        namespace: 'todos',
        label: 'owner',
        targetNamespace: '$users',
        reverseLabel: 'todos',
        cardinality: 'one' as const,
        onDelete: 'cascade' as const,
      },
    ],
    attributes: [
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
        isIndexed: false,
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
        id: 'attr-goal-title',
        namespace: 'goals',
        name: 'title',
        type: 'string' as const,
        isIndexed: false,
        isUnique: false,
        isRequired: false,
        isPrimary: false,
      },
    ],
  };

  const schema: AppSchemaState = {
    namespaces: [todos, goals],
    attributes: [...todos.attributes, ...goals.attributes],
    links: todos.links,
    attrs: {
      'attr-owner': {
        id: 'attr-owner',
        'forward-identity': ['ident-owner', 'todos', 'owner'],
        'reverse-identity': ['ident-owner-reverse', '$users', 'todos'],
        'index?': false,
        'unique?': false,
        cardinality: 'one',
        'value-type': 'ref',
        'on-delete': 'cascade',
      },
      'attr-title': {
        id: 'attr-title',
        'forward-identity': ['ident-title', 'todos', 'title'],
        'index?': true,
        'unique?': false,
        'required?': true,
        cardinality: 'one',
        'value-type': 'blob',
        'checked-data-type': 'string',
      },
    },
    isLoading: false,
  };

  return { client, schema, state: { schema } };
});

vi.mock('@instantdb/react', () => ({ init: () => ({}) }));

vi.mock('@/components/code-editor/code-editor', () => ({
  CodeEditor: () => null,
}));

vi.mock('@/lib/schema/code', () => ({
  SCHEMA_TYPESCRIPT_FILENAME: 'instant.schema.ts',
  schemaTypescriptFile: () => 'const schema = 1;',
  downloadSchemaTypescript: () => undefined,
}));

vi.mock('@/lib/instant', async () => {
  const actual =
    await vi.importActual<typeof import('@/lib/instant')>('@/lib/instant');
  return {
    ...actual,
    useAppClient: () => mocks.client,
    peekAppClient: () => mocks.client,
    useAppSchema: () => mocks.state.schema,
  };
});

beforeEach(() => {
  mocks.state.schema = mocks.schema;
  resetSession();
  resetRuntimeConfig();
  window.localStorage.clear();
  window.sessionStorage.clear();
  setSession({ token: mockSignInToken, user: mockSignInUser });
});

afterEach(() => {
  resetSession();
  resetRuntimeConfig();
  window.localStorage.clear();
  window.sessionStorage.clear();
});

async function renderSchema(route = '/apps/app-1/schema') {
  const result = render({ route });
  await waitFor(() => {
    expect(
      screen.getByRole('heading', { level: 1, name: 'Schema' }),
    ).toBeInTheDocument();
  });
  return result;
}

describe('schema page', () => {
  it.each(['create', 'update', 'delete'] as const)(
    'only warns about public $users %s when it is not registration',
    async (action) => {
      const user = userEvent.setup();
      mocks.state.schema = {
        ...mocks.schema,
        namespaces: [
          ...mocks.schema.namespaces,
          { name: '$users', isSystem: true, attributes: [], links: [] },
        ],
      };
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
      await renderSchema();
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

  it('selects the first namespace and lists its attributes', async () => {
    const { router } = await renderSchema();

    await waitFor(() => {
      expect(router.state.location.search).toMatchObject({ ns: 'todos' });
    });

    const rail = await screen.findByRole('complementary', {
      name: 'Namespaces',
    });
    expect(
      within(rail).getByRole('button', { name: /^todos/ }),
    ).toBeInTheDocument();

    const table = screen.getByRole('table', {
      name: 'Attributes in todos',
    });
    expect(within(table).getByText('title')).toBeInTheDocument();
    expect(within(table).getByText('Required')).toBeInTheDocument();
    expect(within(table).getByText('Indexed')).toBeInTheDocument();
    expect(
      screen.getByRole('button', { name: 'Add attribute' }),
    ).toBeInTheDocument();
  });

  it('switches tabs through the URL', async () => {
    const user = userEvent.setup();
    const { router } = await renderSchema();

    await user.click(screen.getByRole('tab', { name: 'Links' }));

    await waitFor(() => {
      expect(router.state.location.search).toMatchObject({
        ns: 'todos',
        tab: 'links',
      });
    });
    const table = await screen.findByRole('table', {
      name: 'Links from todos',
    });
    expect(within(table).getByText('owner')).toBeInTheDocument();
    expect(within(table).getByText('Cascade')).toBeInTheDocument();
  });

  it('restores the tab and namespace from the URL', async () => {
    await renderSchema('/apps/app-1/schema?ns=goals&tab=code');

    expect(screen.getByRole('tab', { name: 'Code' })).toHaveAttribute(
      'aria-selected',
      'true',
    );
    expect(screen.getByText('instant.schema.ts')).toBeInTheDocument();

    const rail = await screen.findByRole('complementary', {
      name: 'Namespaces',
    });
    expect(
      within(rail).getByRole('button', { name: /^goals/ }),
    ).toHaveAttribute('aria-current', 'true');
  });

  it('stops below admin access and explains the role', async () => {
    await renderSchema('/apps/app-2/schema');

    expect(
      await screen.findByText("You don't have access to schema"),
    ).toBeInTheDocument();
    expect(
      screen.getByText('The schema needs admin access to this app.'),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole('complementary', { name: 'Namespaces' }),
    ).not.toBeInTheDocument();
  });

  it('keeps the empty-schema New namespace control mounted across ticks', async () => {
    mocks.state.schema = {
      namespaces: [],
      attributes: [],
      links: [],
      attrs: {},
      isLoading: false,
    };
    const { router } = await renderSchema();

    const button = await screen.findByRole('button', {
      name: 'New namespace',
    });
    await waitFor(() => {
      expect(router.state.location.search).toMatchObject({ tab: 'attributes' });
      expect(router.state.location.search).not.toHaveProperty('ns');
    });

    await act(async () => {
      await new Promise((resolve) => {
        setTimeout(resolve, 25);
      });
    });

    expect(screen.getByRole('button', { name: 'New namespace' })).toBe(button);
  });
});
