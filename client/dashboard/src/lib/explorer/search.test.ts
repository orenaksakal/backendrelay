import { describe, expect, it } from 'vitest';
import type { FilterAttribute } from '@/components/data/filter-serialization';
import {
  extractTextSearch,
  parseSearchInput,
  textSearchClause,
  withTextSearch,
} from './search';

const ATTRIBUTES: FilterAttribute[] = [
  { name: 'title', type: 'string', indexed: true },
  { name: 'notes', type: 'string', indexed: false },
  { name: 'done', type: 'boolean', indexed: true },
  { name: 'count', type: 'number', indexed: true },
];

describe('explorer search parsing', () => {
  it('treats plain text as a search', () => {
    expect(parseSearchInput('buy milk', ATTRIBUTES)).toEqual({
      kind: 'search',
    });
    expect(parseSearchInput('   ', ATTRIBUTES)).toEqual({ kind: 'search' });
  });

  it('parses colon and comparison quick filters', () => {
    const done = parseSearchInput('done:false', ATTRIBUTES);
    expect(done.kind).toBe('filter');
    if (done.kind !== 'filter') return;
    expect(done.condition).toMatchObject({
      attribute: 'done',
      operator: '$eq',
      value: 'false',
      enabled: true,
    });

    const count = parseSearchInput('count>3', ATTRIBUTES);
    expect(count.kind).toBe('filter');
    if (count.kind !== 'filter') return;
    expect(count.condition).toMatchObject({
      attribute: 'count',
      operator: '$gt',
      value: '3',
    });

    const title = parseSearchInput('title != hello', ATTRIBUTES);
    expect(title.kind).toBe('filter');
    if (title.kind !== 'filter') return;
    expect(title.condition).toMatchObject({
      attribute: 'title',
      operator: '$ne',
      value: 'hello',
    });
  });

  it('explains invalid quick filters inline', () => {
    const unknown = parseSearchInput('missing:1', ATTRIBUTES);
    expect(unknown).toEqual({
      kind: 'invalid',
      message: 'Unknown attribute "missing".',
    });

    const empty = parseSearchInput('done:', ATTRIBUTES);
    expect(empty.kind).toBe('invalid');

    const leading = parseSearchInput(':false', ATTRIBUTES);
    expect(leading.kind).toBe('invalid');

    const unindexed = parseSearchInput('notes>3', ATTRIBUTES);
    expect(unindexed.kind).toBe('invalid');
    if (unindexed.kind !== 'invalid') return;
    expect(unindexed.message).toMatch(/isn't indexed/);
  });

  it('builds an OR of $ilike clauses over indexed strings only', () => {
    expect(textSearchClause('milk', ATTRIBUTES)).toEqual({
      or: [{ title: { $ilike: '%milk%' } }],
    });
    expect(textSearchClause('', ATTRIBUTES)).toBeNull();
    expect(
      textSearchClause('milk', [{ name: 'notes', type: 'string' }]),
    ).toBeNull();
  });

  it('round-trips a generated text search', () => {
    const clause = textSearchClause('milk', ATTRIBUTES);
    const where = { done: true, ...(clause ?? {}) };

    expect(extractTextSearch(where, ATTRIBUTES)).toEqual({
      text: 'milk',
      where: { done: true },
    });
  });

  it('leaves an unfamiliar or clause alone', () => {
    const where = { or: [{ title: { $ilike: '%one%' } }, { done: true }] };
    expect(extractTextSearch(where, ATTRIBUTES)).toEqual({
      text: '',
      where,
    });

    const foreign = { or: [{ owner: { id: 'user-1' } }] };
    expect(extractTextSearch(foreign, ATTRIBUTES)).toEqual({
      text: '',
      where: foreign,
    });
  });

  it('replaces an existing text search instead of stacking or clauses', () => {
    const first = withTextSearch({ done: true }, 'milk', ATTRIBUTES);
    const second = withTextSearch(first, 'bread', ATTRIBUTES);
    expect(second).toEqual({
      done: true,
      or: [{ title: { $ilike: '%bread%' } }],
    });
    expect(withTextSearch(second, '', ATTRIBUTES)).toEqual({ done: true });
  });
});
