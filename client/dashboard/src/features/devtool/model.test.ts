import { describe, expect, it, vi } from 'vitest';
import {
  DEVTOOL_CLOSE_MESSAGE,
  closeDevtool,
  defaultInstaqlQuery,
  parseInstaqlQuery,
  playgroundCell,
  playgroundStatus,
  playgroundTable,
  queryResultCount,
  queryResultError,
  queryResultHasData,
} from './model';

describe('devtool close protocol', () => {
  it('posts the message the SDK listens for', () => {
    const postMessage = vi.fn();
    closeDevtool({ postMessage });

    expect(postMessage).toHaveBeenCalledWith(DEVTOOL_CLOSE_MESSAGE, '*');
    expect(DEVTOOL_CLOSE_MESSAGE).toEqual({ type: 'close' });
  });
});

describe('defaultInstaqlQuery', () => {
  it('picks the first user namespace over a system one', () => {
    expect(
      defaultInstaqlQuery([
        { name: '$users', isSystem: true },
        { name: 'todos', isSystem: false },
      ]),
    ).toBe('{\n  "todos": {}\n}');
  });

  it('falls back to a system namespace, then to an empty object', () => {
    expect(defaultInstaqlQuery([{ name: '$users', isSystem: true }])).toBe(
      '{\n  "$users": {}\n}',
    );
    expect(defaultInstaqlQuery([])).toBe('{}');
  });
});

describe('parseInstaqlQuery', () => {
  it('accepts a namespace document', () => {
    expect(parseInstaqlQuery('{"todos": {}}')).toEqual({
      ok: true,
      query: { todos: {} },
    });
  });

  it('reports invalid JSON', () => {
    expect(parseInstaqlQuery('{')).toEqual({
      ok: false,
      message: 'The query must be valid JSON.',
    });
  });

  it('rejects arrays, null and empty documents', () => {
    expect(parseInstaqlQuery('[]')).toEqual({
      ok: false,
      message: 'The query must be a JSON object, for example { "todos": {} }.',
    });
    expect(parseInstaqlQuery('null').ok).toBe(false);
    expect(parseInstaqlQuery('{}')).toEqual({
      ok: false,
      message: 'Enter at least one namespace to query.',
    });
  });
});

describe('query result helpers', () => {
  const result = {
    data: {
      todos: [{ id: 't1' }, { id: 't2' }],
      goals: [{ id: 'g1' }],
      $users: [],
    },
  };

  it('counts entities across namespaces', () => {
    expect(queryResultCount(result)).toBe(3);
    expect(queryResultCount({ data: null })).toBe(0);
    expect(queryResultCount(null)).toBe(0);
  });

  it('waits for a data payload before showing a result', () => {
    expect(queryResultHasData(result)).toBe(true);
    expect(queryResultHasData({ data: {} })).toBe(true);
    expect(queryResultHasData({ loading: true })).toBe(false);
    expect(queryResultHasData({ data: null })).toBe(false);
    expect(queryResultHasData(undefined)).toBe(false);
  });

  it('reads the server error message', () => {
    expect(queryResultError({ error: { message: 'Permission denied.' } })).toBe(
      'Permission denied.',
    );
    expect(queryResultError({ error: {} })).toBe('The query failed.');
    expect(queryResultError(result)).toBeNull();
    expect(queryResultError(undefined)).toBeNull();
  });

  it('formats the status line with a singular entity', () => {
    expect(playgroundStatus(1, 4)).toBe('1 entity · 4 ms');
    expect(playgroundStatus(1204, 14)).toBe('1,204 entities · 14 ms');
  });

  it('builds the table from the first namespace and puts id first', () => {
    const table = playgroundTable(result);
    expect(table?.namespace).toBe('todos');
    expect(table?.columns).toEqual(['id']);
    expect(table?.rows).toHaveLength(2);

    expect(
      playgroundTable({
        data: { todos: [{ title: 'Buy milk', id: 't1', done: false }] },
      })?.columns,
    ).toEqual(['id', 'title', 'done']);
    expect(playgroundTable({ data: { todos: [] } })).toEqual({
      namespace: 'todos',
      columns: [],
      rows: [],
    });
    expect(playgroundTable({ data: {} })).toBeNull();
    expect(playgroundTable(null)).toBeNull();
  });

  it('formats table cells', () => {
    expect(playgroundCell('Buy milk')).toBe('Buy milk');
    expect(playgroundCell(4)).toBe('4');
    expect(playgroundCell(false)).toBe('false');
    expect(playgroundCell(null)).toBe('null');
    expect(playgroundCell(undefined)).toBe('unset');
    expect(playgroundCell({ link: true })).toBe('{"link":true}');
  });
});
