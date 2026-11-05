import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { HttpResponse, http } from 'msw';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { resetSession, setSession } from '@/lib/auth/session';
import type { AppInstantClient } from '@/lib/instant';
import { resetRuntimeConfig } from '@/lib/runtime-config';
import { mockSignInToken, mockSignInUser } from '@/test/fixtures/api/auth';
import { mockQueryPermissionsCheck } from '@/test/fixtures/api/apps';
import { server } from '@/test/msw/node';
import { render } from '@/test/render';

const API = 'http://localhost:8888';
const ROUTE = '/apps/app-1/playground';

const mocks = vi.hoisted(() => {
  const queries: Record<string, unknown>[] = [];

  const client = {
    core: {
      shutdown: () => undefined,
      subscribeQuery: (
        query: Record<string, unknown>,
        callback: (result: unknown) => void,
      ) => {
        queries.push(query);
        callback({
          data: {
            todos: [
              { id: 'todo-1', title: 'Buy milk' },
              { id: 'todo-2', title: 'Walk the dog' },
            ],
          },
          pageInfo: { todos: { hasNextPage: false } },
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

  const schema = {
    namespaces: [
      {
        name: 'todos',
        isSystem: false,
        attributes: [
          {
            id: 'attr-id',
            namespace: 'todos',
            name: 'id',
            type: 'string' as const,
            isIndexed: false,
            isUnique: false,
            isRequired: false,
            isPrimary: true,
          },
          {
            id: 'attr-title',
            namespace: 'todos',
            name: 'title',
            type: 'string' as const,
            isIndexed: false,
            isUnique: false,
            isRequired: false,
            isPrimary: false,
          },
        ],
        links: [],
      },
    ],
    attributes: [],
    links: [],
    attrs: null,
    isLoading: false,
  };

  return { client, schema, queries };
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

vi.mock('@/components/code-editor/code-editor', () => ({
  CodeEditor: (props: {
    value: string;
    onChange?: (value: string) => void;
    mode: string;
    'aria-label': string;
  }) => (
    <div>
      <textarea
        aria-label={props['aria-label']}
        value={props.value}
        onChange={(event) => {
          props.onChange?.(event.target.value);
        }}
      />
      <span data-testid="editor-mode">{props.mode}</span>
    </div>
  ),
}));

beforeEach(() => {
  resetSession();
  resetRuntimeConfig();
  window.localStorage.clear();
  mocks.queries.length = 0;
  setSession({ token: mockSignInToken, user: mockSignInUser });
});

afterEach(() => {
  resetSession();
  resetRuntimeConfig();
  window.localStorage.clear();
});

async function renderPlayground(route = ROUTE) {
  const result = render({ route });
  await waitFor(() => {
    expect(
      screen.getByRole('heading', { level: 1, name: 'Playground' }),
    ).toBeInTheDocument();
  });
  return result;
}

function queryEditor(): HTMLTextAreaElement {
  return screen.getByLabelText<HTMLTextAreaElement>('InstaQL query');
}

describe('playground page', () => {
  it('runs an admin query live through the shared client', async () => {
    await renderPlayground();

    await waitFor(() => {
      expect(queryEditor().value).toContain('"todos"');
    });
    expect(screen.getByTestId('editor-mode')).toHaveTextContent('instaql');
    expect(
      screen.getByRole('switch', { name: 'Live updates (Admin runs)' }),
    ).toBeChecked();

    await userEvent
      .setup()
      .click(screen.getByRole('button', { name: /^Run (Ctrl|⌘)/ }));

    expect(await screen.findByRole('status')).toHaveTextContent('2 entities ·');
    expect(screen.getByRole('status')).toHaveTextContent('live');
    expect(
      within(screen.getByRole('group', { name: 'Query result' })).getByText(
        '"Buy milk"',
      ),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole('tab', { name: 'Checks' }),
    ).not.toBeInTheDocument();
    expect(mocks.queries).toHaveLength(1);

    const stored = JSON.parse(
      window.localStorage.getItem('br.playground.app-1') ?? '[]',
    ) as unknown[];
    expect(stored).toHaveLength(1);

    await userEvent
      .setup()
      .click(screen.getByRole('button', { name: 'Run history' }));
    const history = await screen.findByRole('group', { name: 'Runs' });
    expect(within(history).getByRole('menuitem')).toHaveTextContent('todos');
  });

  it('runs as a guest and shows the debug checks', async () => {
    const user = userEvent.setup();
    let headers: Headers | undefined;
    let body: unknown;
    server.use(
      http.post(`${API}/admin/query_perms_check`, async ({ request }) => {
        headers = request.headers;
        body = await request.json();
        return HttpResponse.json(mockQueryPermissionsCheck);
      }),
    );

    await renderPlayground(`${ROUTE}?as=guest`);
    await waitFor(() => {
      expect(queryEditor().value).toContain('"todos"');
    });
    expect(
      screen.getByRole('switch', { name: 'Live updates (Admin runs)' }),
    ).toHaveAttribute('aria-disabled', 'true');

    await user.click(screen.getByRole('button', { name: /^Run (Ctrl|⌘)/ }));

    expect(await screen.findByRole('status')).toHaveTextContent('1 entity ·');
    expect(headers?.get('as-guest')).toBe('true');
    expect(body).toMatchObject({
      query: { todos: { $: { limit: 10 } } },
      'inference?': false,
    });

    await user.click(screen.getByRole('tab', { name: 'Checks' }));
    expect(await screen.findByText('auth.id != null')).toBeInTheDocument();
    expect(screen.getByText('✓ allowed')).toBeInTheDocument();
  });

  it('runs transaction steps and shows the response', async () => {
    const user = userEvent.setup();
    let body: unknown;
    server.use(
      http.post(`${API}/admin/transact_perms_check`, async ({ request }) => {
        body = await request.json();
        return HttpResponse.json({
          'tx-id': 42,
          'all-checks-ok?': true,
          'committed?': true,
          'check-results': [
            {
              etype: 'todos',
              eid: 'todo-1',
              action: 'update',
              program: {
                action: 'update',
                'display-code': 'auth.id != null',
              },
              'check-pass?': true,
            },
          ],
        });
      }),
    );

    await renderPlayground(`${ROUTE}?mode=transact&as=admin`);

    expect(
      screen.getByText(/Transactions change real data in todo-app/),
    ).toBeInTheDocument();
    const editor =
      screen.getByLabelText<HTMLTextAreaElement>('Transaction steps');
    fireEvent.change(editor, {
      target: {
        value: JSON.stringify(
          [['update', 'todos', 'todo-1', { title: 'Done' }]],
          null,
          2,
        ),
      },
    });

    await user.click(screen.getByRole('button', { name: /^Run (Ctrl|⌘)/ }));

    expect(await screen.findByRole('status')).toHaveTextContent('Committed');
    expect(
      screen.getByText('Every permission check passed.'),
    ).toBeInTheDocument();
    expect(body).toMatchObject({
      steps: [['update', 'todos', 'todo-1', { title: 'Done' }]],
      'dangerously-commit-tx': true,
    });

    await user.click(screen.getByRole('tab', { name: 'Response' }));
    expect(await screen.findByText(/"tx-id": 42/)).toBeInTheDocument();
  });

  it('opens the custom rules editor from the switch', async () => {
    const user = userEvent.setup();
    await renderPlayground();

    await user.click(screen.getByRole('switch', { name: 'Use custom rules' }));

    const sheet = await screen.findByRole('dialog');
    expect(
      within(sheet).getByRole('heading', { name: 'Custom permission rules' }),
    ).toBeInTheDocument();
    expect(
      within(sheet).getByLabelText('Custom permission rules'),
    ).toBeInTheDocument();

    await user.click(within(sheet).getByRole('button', { name: 'Done' }));
    await waitFor(() => {
      expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    });
  });
});
