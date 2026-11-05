import { describe, expect, it } from 'vitest';
import type { FilterAttribute } from '@/components/data/filter-serialization';
import {
  DEFAULT_EXPLORER_PAGE_SIZE,
  explorerHref,
  explorerSearchFor,
  explorerSearchSchema,
  readExplorerUrlState,
  resolveSort,
  serializeExplorerWhere,
} from './url-state';

const ATTRIBUTES: FilterAttribute[] = [
  { name: 'id', type: 'string', indexed: false },
  { name: 'title', type: 'string', indexed: true },
  { name: 'done', type: 'boolean', indexed: true },
  { name: 'createdAt', type: 'date', indexed: true },
];

describe('explorer URL state', () => {
  it('validates search params with safe defaults', () => {
    expect(explorerSearchSchema.parse({})).toEqual({
      dir: 'asc',
      size: DEFAULT_EXPLORER_PAGE_SIZE,
    });
    expect(
      explorerSearchSchema.parse({ ns: 'todos', size: '200', dir: 'desc' }),
    ).toMatchObject({ ns: 'todos', size: 200, dir: 'desc' });

    const invalid = explorerSearchSchema.parse({
      ns: '',
      size: 'huge',
      dir: 'sideways',
      where: 42,
    });
    expect(invalid.ns).toBeUndefined();
    expect(invalid.where).toBeUndefined();
    expect(invalid.dir).toBe('asc');
    expect(invalid.size).toBe(DEFAULT_EXPLORER_PAGE_SIZE);
  });

  it('round-trips filters, text search, sort and size through the URL', () => {
    const state = {
      namespace: 'todos',
      text: 'milk',
      conditions: [
        {
          id: 'filter-1',
          enabled: true,
          attribute: 'done',
          operator: '$eq' as const,
          value: 'false',
        },
        {
          id: 'filter-2',
          enabled: true,
          attribute: 'createdAt',
          operator: '$gte' as const,
          value: '2026-01-01',
        },
      ],
      sort: { attribute: 'createdAt', direction: 'desc' as const },
      size: 200,
    };

    const search = explorerSearchFor(state, ATTRIBUTES);
    expect(search).toMatchObject({
      ns: 'todos',
      sort: 'createdAt',
      dir: 'desc',
      size: 200,
    });

    const where: unknown = JSON.parse(search.where ?? '{}');
    expect(where).toEqual({
      done: false,
      createdAt: { $gte: '2026-01-01' },
      or: [{ title: { $ilike: '%milk%' } }],
    });

    const round = readExplorerUrlState(
      explorerSearchSchema.parse(search),
      ATTRIBUTES,
    );
    expect(round.namespace).toBe('todos');
    expect(round.text).toBe('milk');
    expect(round.size).toBe(200);
    expect(round.sort).toEqual({ attribute: 'createdAt', direction: 'desc' });
    expect(round.conditions).toHaveLength(2);
    expect(round.conditions[0]).toMatchObject({
      attribute: 'done',
      operator: '$eq',
      value: 'false',
    });
    expect(round.conditions[1]).toMatchObject({
      attribute: 'createdAt',
      operator: '$gte',
      value: '2026-01-01',
    });
  });

  it('omits an empty where from the URL', () => {
    const search = explorerSearchFor(
      {
        namespace: 'todos',
        text: '',
        conditions: [],
        sort: null,
        size: DEFAULT_EXPLORER_PAGE_SIZE,
      },
      ATTRIBUTES,
    );
    expect(search.where).toBeUndefined();
    expect(
      serializeExplorerWhere({ text: '', conditions: [] }, ATTRIBUTES),
    ).toBeNull();
  });

  it('ignores malformed where and unknown params', () => {
    const state = readExplorerUrlState(
      explorerSearchSchema.parse({ ns: 'todos', where: 'not-json' }),
      ATTRIBUTES,
    );
    expect(state.conditions).toEqual([]);
    expect(state.text).toBe('');
  });

  it('drops a sort whose attribute is not in the namespace', () => {
    expect(
      resolveSort({ attribute: 'createdAt', direction: 'asc' }, ATTRIBUTES),
    ).toEqual({ attribute: 'createdAt', direction: 'asc' });
    expect(
      resolveSort({ attribute: 'missing', direction: 'asc' }, ATTRIBUTES),
    ).toBeNull();
    expect(resolveSort(null, ATTRIBUTES)).toBeNull();
  });

  it('builds an Explorer link with a namespace and optional id filter', () => {
    expect(explorerHref('app-1', 'todos')).toBe(
      '/apps/app-1/explorer?ns=todos',
    );
    expect(explorerHref('app-1', 'goals', { id: 'goal-1' })).toContain(
      '/apps/app-1/explorer?ns=goals&where=',
    );
  });

  it('round-trips the entity sheet target through the URL', () => {
    const base = {
      namespace: 'todos',
      text: '',
      conditions: [],
      sort: null,
      size: DEFAULT_EXPLORER_PAGE_SIZE,
    };

    const editing = explorerSearchFor(
      { ...base, entity: 't1', creating: false },
      ATTRIBUTES,
    );
    expect(editing.entity).toBe('t1');
    expect(editing.new).toBeUndefined();

    const creating = explorerSearchFor(
      { ...base, entity: null, creating: true },
      ATTRIBUTES,
    );
    expect(creating.entity).toBeUndefined();
    expect(creating.new).toBe('1');

    const opened = readExplorerUrlState(
      explorerSearchSchema.parse({ ns: 'todos', entity: 't1' }),
      ATTRIBUTES,
    );
    expect(opened.entity).toBe('t1');
    expect(opened.creating).toBe(false);

    const fresh = readExplorerUrlState(
      explorerSearchSchema.parse({ ns: 'todos', new: '1' }),
      ATTRIBUTES,
    );
    expect(fresh.entity).toBeNull();
    expect(fresh.creating).toBe(true);

    expect(explorerSearchSchema.parse({ new: '2' }).new).toBeUndefined();
  });
});
