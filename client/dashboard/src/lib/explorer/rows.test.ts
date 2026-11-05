import { describe, expect, it } from 'vitest';
import {
  buildCountsQuery,
  buildExplorerQuery,
  parseExplorerError,
  parseExplorerResult,
  readAggregateCounts,
} from './rows';

describe('explorer queries', () => {
  it('reads rows and the next-page flag for the namespace', () => {
    expect(
      parseExplorerResult(
        {
          data: { todos: [{ id: 't1' }, { id: 't2' }, 'not-a-row'] },
          pageInfo: { todos: { hasNextPage: true } },
        },
        'todos',
      ),
    ).toEqual({
      rows: [{ id: 't1' }, { id: 't2' }],
      hasNextPage: true,
    });

    expect(parseExplorerResult({ data: {} }, 'todos')).toEqual({
      rows: [],
      hasNextPage: false,
    });
    expect(parseExplorerResult(undefined, 'todos')).toEqual({
      rows: [],
      hasNextPage: false,
    });
  });

  it('surfaces the query error message', () => {
    expect(
      parseExplorerError({ error: { message: 'Permission denied.' } }),
    ).toBe('Permission denied.');
    expect(parseExplorerError({ error: {} })).toBe('The query failed.');
    expect(parseExplorerError({ data: {} })).toBeNull();
    expect(parseExplorerError(undefined)).toBeNull();
  });

  it('builds the paged, filtered, ordered InstaQL query', () => {
    expect(
      buildExplorerQuery('todos', {
        where: {},
        sort: null,
        limit: 100,
      }),
    ).toEqual({ todos: { $: { limit: 100 } } });

    expect(
      buildExplorerQuery('todos', {
        where: { done: false },
        sort: { attribute: 'createdAt', direction: 'desc' },
        limit: 100,
      }),
    ).toEqual({
      todos: {
        $: {
          limit: 100,
          where: { done: false },
          order: { createdAt: 'desc' },
        },
      },
    });
  });

  it('asks for link targets as id-only fields', () => {
    expect(
      buildExplorerQuery('todos', {
        where: {},
        sort: null,
        limit: 100,
        linkLabels: ['owner', 'goals'],
      }),
    ).toEqual({
      todos: {
        owner: { $: { fields: ['id'] } },
        goals: { $: { fields: ['id'] } },
        $: { limit: 100 },
      },
    });
  });

  it('asks for one aggregate count per namespace', () => {
    expect(buildCountsQuery(['todos', '$users'])).toEqual({
      todos: { $: { aggregate: 'count' } },
      $users: { $: { aggregate: 'count' } },
    });

    expect(
      readAggregateCounts(
        { aggregate: { todos: { count: 1204 }, $users: { count: 3 } } },
        ['todos', '$users', 'goals'],
      ),
    ).toEqual({ todos: 1204, $users: 3 });
    expect(readAggregateCounts({ data: {} }, ['todos'])).toEqual({});
  });
});
