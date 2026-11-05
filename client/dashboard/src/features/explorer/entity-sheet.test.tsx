import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type {
  AppAttribute,
  AppInstantClient,
  AppNamespace,
  AppTxChunk,
} from '@/lib/instant';
import { writeDraft } from '@/lib/explorer/drafts';
import { EntitySheet } from './entity-sheet';

vi.mock('@/components/code-editor/code-editor', () => ({
  CodeEditor: ({
    value,
    'aria-label': ariaLabel,
  }: {
    value: string;
    'aria-label': string;
  }) => <textarea aria-label={ariaLabel} value={value} readOnly />,
}));

function attribute(
  name: string,
  type: AppAttribute['type'],
  overrides: Partial<AppAttribute> = {},
): AppAttribute {
  return {
    id: `attr-${name}`,
    namespace: 'todos',
    name,
    type,
    isIndexed: false,
    isUnique: false,
    isRequired: false,
    isPrimary: false,
    ...overrides,
  };
}

const TODOS: AppNamespace = {
  name: 'todos',
  isSystem: false,
  links: [],
  attributes: [
    attribute('id', 'string', { isUnique: true, isPrimary: true }),
    attribute('title', 'string', { isRequired: true }),
    attribute('done', 'boolean'),
    attribute('count', 'number'),
    attribute('note', 'string'),
  ],
};

interface RecordedCall {
  op: string;
  values?: unknown;
}

function fakeClient(row: Record<string, unknown> | null) {
  const calls: RecordedCall[] = [];
  const transactions: unknown[] = [];
  const unsetOps: unknown[][] = [];
  const chunk: AppTxChunk = {
    update: (values) => {
      calls.push({ op: 'update', values });
      return chunk;
    },
    link: (values) => {
      calls.push({ op: 'link', values });
      return chunk;
    },
    unlink: (values) => {
      calls.push({ op: 'unlink', values });
      return chunk;
    },
    delete: () => {
      calls.push({ op: 'delete' });
      return chunk;
    },
  };
  const client: AppInstantClient = {
    core: {
      shutdown: () => undefined,
      subscribeQuery: (query, callback) => {
        if (row === null) return () => undefined;
        const namespace = Object.keys(query)[0] ?? 'todos';
        callback({
          data: { [namespace]: [row] },
          pageInfo: { [namespace]: { hasNextPage: false } },
        });
        return () => undefined;
      },
      _reactor: {
        subscribeAttrs: () => () => undefined,
        pushOps: (ops) => {
          unsetOps.push(...ops.map((op) => [...op]));
          return Promise.resolve();
        },
      },
    },
    tx: {
      todos: new Proxy<Record<string, AppTxChunk>>({}, { get: () => chunk }),
    },
    transact: (chunks) => {
      transactions.push(chunks);
      return Promise.resolve();
    },
  };
  return { client, calls, transactions, unsetOps };
}

function renderSheet(
  options: {
    entityId?: string | null;
    row?: Record<string, unknown> | null;
  } = {},
) {
  const { entityId = 't1', row = null } = options;
  const fake = fakeClient(row);
  const onSaved =
    vi.fn<(entityId: string, mode: 'close' | 'another') => void>();
  const onOpenChange = vi.fn();
  render(
    <EntitySheet
      appId="app-1"
      namespace={TODOS}
      namespaces={[TODOS]}
      client={fake.client}
      entityId={entityId}
      onOpenChange={onOpenChange}
      onSaved={onSaved}
    />,
  );
  return { ...fake, onSaved, onOpenChange };
}

beforeEach(() => {
  window.sessionStorage.clear();
});

afterEach(() => {
  window.sessionStorage.clear();
});

describe('entity sheet', () => {
  it('saves edited fields through the app client', async () => {
    const user = userEvent.setup();
    const { calls, transactions, onSaved } = renderSheet({
      row: { id: 't1', title: 'Buy milk', done: false, count: 2 },
    });

    const title = await screen.findByRole('textbox', { name: 'title' });
    expect(title).toHaveValue('Buy milk');

    await user.clear(title);
    await user.type(title, 'Ship v2');
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => {
      expect(transactions).toHaveLength(1);
    });
    const update = calls.find((call) => call.op === 'update');
    expect(update?.values as Record<string, unknown>).toMatchObject({
      title: 'Ship v2',
      done: false,
      count: 2,
    });
    expect(onSaved).toHaveBeenCalledWith('t1', 'close');
  });

  it('requires a required field before saving', async () => {
    const user = userEvent.setup();
    const { transactions } = renderSheet({ entityId: null });

    await user.click(screen.getByRole('button', { name: 'Save' }));

    expect(
      screen.getAllByText('This field is required.').length,
    ).toBeGreaterThan(0);
    expect(
      screen.getByText('Fix the highlighted fields before saving.'),
    ).toBeInTheDocument();
    expect(transactions).toHaveLength(0);
  });

  it('creates a new entity and reports its generated id', async () => {
    const user = userEvent.setup();
    const { calls, onSaved } = renderSheet({ entityId: null });

    await user.type(screen.getByRole('textbox', { name: 'title' }), 'New todo');
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => {
      expect(onSaved).toHaveBeenCalledTimes(1);
    });
    const update = calls.find((call) => call.op === 'update');
    expect(update?.values).toMatchObject({ title: 'New todo' });
    const [id, mode] = onSaved.mock.calls[0] ?? [];
    expect(typeof id).toBe('string');
    expect(mode).toBe('close');
  });

  it('retracts an attribute when its field is cleared', async () => {
    const user = userEvent.setup();
    const { calls, unsetOps } = renderSheet({
      row: { id: 't1', title: 'Buy milk', note: 'hello' },
    });

    await screen.findByRole('textbox', { name: 'title' });
    await user.click(screen.getByRole('button', { name: 'Clear note' }));
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => {
      expect(unsetOps).toHaveLength(1);
    });
    expect(unsetOps[0]).toEqual(['retract-triple', 't1', 'attr-note', 'hello']);
    const update = calls.find((call) => call.op === 'update');
    expect(update?.values as Record<string, unknown>).not.toHaveProperty(
      'note',
    );
  });

  it('offers to restore a draft from earlier', async () => {
    const user = userEvent.setup();
    writeDraft('app-1', 'todos', 't1', {
      values: { title: 'Draft title' },
      unset: [],
      json: {},
      links: {},
    });
    renderSheet({ row: { id: 't1', title: 'Buy milk' } });

    expect(
      await screen.findByText('You have unsaved changes from earlier'),
    ).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Restore' }));

    expect(screen.getByRole('textbox', { name: 'title' })).toHaveValue(
      'Draft title',
    );
  });

  it('discards a draft instead of applying it', async () => {
    const user = userEvent.setup();
    writeDraft('app-1', 'todos', 't1', {
      values: { title: 'Draft title' },
      unset: [],
      json: {},
      links: {},
    });
    renderSheet({ row: { id: 't1', title: 'Buy milk' } });

    await screen.findByText('You have unsaved changes from earlier');
    await user.click(screen.getByRole('button', { name: 'Discard' }));

    expect(
      screen.queryByText('You have unsaved changes from earlier'),
    ).not.toBeInTheDocument();
    expect(screen.getByRole('textbox', { name: 'title' })).toHaveValue(
      'Buy milk',
    );
  });

  it('reads read-only entity JSON with a copy action', async () => {
    const user = userEvent.setup();
    renderSheet({ row: { id: 't1', title: 'Buy milk' } });

    await screen.findByRole('textbox', { name: 'title' });
    await user.click(screen.getByRole('tab', { name: 'JSON' }));

    expect(
      screen.getByRole('button', { name: 'Copy entity JSON' }),
    ).toBeInTheDocument();
    expect(screen.getByRole('textbox', { name: 'Entity JSON' })).toHaveValue(
      JSON.stringify({ id: 't1', title: 'Buy milk' }, null, 2),
    );
  });
});
