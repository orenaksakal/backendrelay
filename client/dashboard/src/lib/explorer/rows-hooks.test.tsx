import { act, renderHook } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import type { AppInstantClient } from '@/lib/instant';
import { useExplorerRows, useNamespaceCounts, useExplorerEntity } from './rows';

/**
 * The hook behaviour the grid relies on: the first page always shows, later
 * results wait behind Refresh (paused or sorted/filtered), and a burst pauses
 * live data (B.5.5).
 */

interface FakeClient extends AppInstantClient {
  emit: (result: unknown) => void;
  queries: Record<string, unknown>[];
}

function createClient(): FakeClient {
  const listeners: ((result: unknown) => void)[] = [];
  const queries: Record<string, unknown>[] = [];
  return {
    core: {
      shutdown: () => undefined,
      subscribeQuery: (query, callback) => {
        queries.push(query);
        listeners.push(callback);
        return () => undefined;
      },
      _reactor: {
        subscribeAttrs: () => () => undefined,
        pushOps: () => Promise.resolve(undefined),
      },
    },
    tx: {},
    transact: () => Promise.resolve(undefined),
    queries,
    emit: (result) => {
      for (const listener of listeners) listener(result);
    },
  };
}

function rowsResult(ids: string[], hasNextPage = false) {
  return {
    data: { todos: ids.map((id) => ({ id })) },
    pageInfo: { todos: { hasNextPage } },
  };
}

function renderRows(
  client: FakeClient,
  options: {
    sort?: { attribute: string; direction: 'asc' | 'desc' } | null;
    where?: Record<string, unknown>;
  } = {},
) {
  return renderHook(() =>
    useExplorerRows({
      client,
      namespace: 'todos',
      where: options.where ?? {},
      sort: options.sort ?? null,
      limit: 100,
    }),
  );
}

describe('useExplorerRows', () => {
  it('shows the first page with its next-page flag', () => {
    const client = createClient();
    const { result } = renderRows(client);

    act(() => {
      client.emit(rowsResult(['t1'], true));
    });

    expect(result.current.rows).toEqual([{ id: 't1' }]);
    expect(result.current.hasNextPage).toBe(true);
    expect(result.current.isLoading).toBe(false);
    expect(result.current.error).toBeNull();
  });

  it('holds updates while paused and applies them on resume', () => {
    const client = createClient();
    const { result } = renderRows(client);

    act(() => {
      client.emit(rowsResult(['t1']));
    });
    act(() => {
      result.current.togglePause();
    });
    expect(result.current.paused).toBe(true);

    act(() => {
      client.emit(rowsResult(['t1', 't2']));
    });
    expect(result.current.pending).toBe(true);
    expect(result.current.rows).toHaveLength(1);

    act(() => {
      result.current.resume();
    });
    expect(result.current.rows).toHaveLength(2);
    expect(result.current.pending).toBe(false);
  });

  it('does not reorder a sorted list under the cursor', () => {
    const client = createClient();
    const { result } = renderRows(client, {
      sort: { attribute: 'title', direction: 'asc' },
    });

    act(() => {
      client.emit(rowsResult(['t1']));
    });
    act(() => {
      client.emit(rowsResult(['t2', 't1']));
    });
    expect(result.current.pending).toBe(true);
    expect(result.current.rows).toEqual([{ id: 't1' }]);

    act(() => {
      result.current.refresh();
    });
    expect(result.current.rows).toEqual([{ id: 't2' }, { id: 't1' }]);
    expect(result.current.pending).toBe(false);
  });

  it('pauses itself when updates exceed ten per second', () => {
    const client = createClient();
    const { result } = renderRows(client);

    act(() => {
      for (let index = 0; index < 11; index += 1) {
        client.emit(rowsResult([`t${String(index)}`]));
      }
    });

    expect(result.current.volumePaused).toBe(true);
    expect(result.current.paused).toBe(true);

    act(() => {
      result.current.resume();
    });
    expect(result.current.volumePaused).toBe(false);
    expect(result.current.paused).toBe(false);
  });

  it('surfaces a query error and retries', () => {
    const client = createClient();
    const { result } = renderRows(client);

    act(() => {
      client.emit({ error: { message: 'Permission denied.' } });
    });
    expect(result.current.error).toBe('Permission denied.');

    act(() => {
      result.current.refresh();
    });
    expect(result.current.error).toBeNull();
  });
});

describe('useNamespaceCounts', () => {
  it('reads one aggregate count per namespace', () => {
    const client = createClient();
    const { result } = renderHook(() =>
      useNamespaceCounts(client, ['todos', 'goals']),
    );

    act(() => {
      client.emit({ aggregate: { todos: { count: 1204 } } });
    });

    expect(result.current).toEqual({ todos: 1204 });
  });
});

describe('useExplorerEntity', () => {
  it('subscribes to one entity by id and exposes the row', () => {
    const client = createClient();
    const { result } = renderHook(() =>
      useExplorerEntity(client, 'todos', 't1'),
    );

    expect(result.current.isLoading).toBe(true);
    expect(client.queries[0]).toEqual({
      todos: { $: { where: { id: 't1' }, limit: 1 } },
    });

    act(() => {
      client.emit({
        data: { todos: [{ id: 't1', title: 'Buy milk' }] },
        pageInfo: { todos: { hasNextPage: false } },
      });
    });

    expect(result.current.entity).toEqual({ id: 't1', title: 'Buy milk' });
    expect(result.current.isLoading).toBe(false);
    expect(result.current.error).toBeNull();
  });

  it('surfaces a query error for the sheet', () => {
    const client = createClient();
    const { result } = renderHook(() =>
      useExplorerEntity(client, 'todos', 't1'),
    );

    act(() => {
      client.emit({ error: { message: 'Entity not found.' } });
    });

    expect(result.current.error).toBe('Entity not found.');
    expect(result.current.entity).toBeNull();
  });

  it('stays idle without an entity id', () => {
    const client = createClient();
    const { result } = renderHook(() =>
      useExplorerEntity(client, 'todos', null),
    );

    expect(client.queries).toEqual([]);
    expect(result.current.isLoading).toBe(false);
    expect(result.current.entity).toBeNull();
  });
});
