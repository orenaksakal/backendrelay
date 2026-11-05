import { describe, expect, it } from 'vitest';
import {
  createFilterCondition,
  parseWhereParam,
  serializeFilters,
  serializeWhereParam,
  type FilterAttribute,
  type FilterCondition,
} from './filter-serialization';

const ATTRIBUTES: FilterAttribute[] = [
  { name: 'title', type: 'string', indexed: true },
  { name: 'done', type: 'boolean' },
  { name: 'count', type: 'number', indexed: true },
  { name: 'meta', type: 'json' },
];

function condition(patch: Partial<FilterCondition>): FilterCondition {
  return { ...createFilterCondition(), ...patch };
}

describe('FilterPanel serialization', () => {
  it('writes $eq as the implicit attribute value', () => {
    const where = serializeFilters(
      [condition({ attribute: 'done', value: 'false' })],
      ATTRIBUTES,
    );

    expect(where).toEqual({ done: false });
  });

  it('coerces values by attribute type', () => {
    const where = serializeFilters(
      [condition({ attribute: 'count', value: '42' })],
      ATTRIBUTES,
    );

    expect(where).toEqual({ count: 42 });
  });

  it('merges operators on one attribute into a single object', () => {
    const where = serializeFilters(
      [
        condition({ attribute: 'count', operator: '$gt', value: '3' }),
        condition({ attribute: 'count', operator: '$lte', value: '10' }),
      ],
      ATTRIBUTES,
    );

    expect(where).toEqual({ count: { $gt: 3, $lte: 10 } });
  });

  it('keeps an explicit $eq when an operator joins the same attribute', () => {
    const where = serializeFilters(
      [
        condition({ attribute: 'done', value: 'false' }),
        condition({ attribute: 'done', operator: '$ne', value: 'true' }),
      ],
      ATTRIBUTES,
    );

    expect(where).toEqual({ done: { $eq: false, $ne: true } });
  });

  it('splits $in lists and coerces each entry', () => {
    const where = serializeFilters(
      [condition({ attribute: 'count', operator: '$in', value: '3, 5' })],
      ATTRIBUTES,
    );

    expect(where).toEqual({ count: { $in: [3, 5] } });
  });

  it('serializes $isNull without needing a value', () => {
    const where = serializeFilters(
      [condition({ attribute: 'title', operator: '$isNull', value: '' })],
      ATTRIBUTES,
    );

    expect(where).toEqual({ title: { $isNull: true } });
  });

  it('skips disabled rows, empty values and unknown attributes', () => {
    const where = serializeFilters(
      [
        condition({ attribute: 'title', value: 'todo', enabled: false }),
        condition({ attribute: 'done', value: '' }),
        condition({ attribute: 'missing', value: 'x' }),
        condition({ attribute: '', value: 'x' }),
      ],
      ATTRIBUTES,
    );

    expect(where).toEqual({});
  });

  it('round-trips through the URL parameter', () => {
    const conditions = [
      condition({ attribute: 'title', operator: '$ilike', value: '%milk%' }),
      condition({ attribute: 'count', operator: '$gt', value: '2' }),
    ];

    const param = serializeWhereParam(conditions, ATTRIBUTES);
    expect(param).toBe('{"title":{"$ilike":"%milk%"},"count":{"$gt":2}}');

    const parsed = parseWhereParam(param, ATTRIBUTES);
    expect(serializeFilters(parsed, ATTRIBUTES)).toEqual(
      serializeFilters(conditions, ATTRIBUTES),
    );
    expect(parsed.map((entry) => entry.operator)).toEqual(['$ilike', '$gt']);
  });

  it('returns no filters for an empty or malformed parameter', () => {
    expect(serializeWhereParam([], ATTRIBUTES)).toBe('');
    expect(parseWhereParam('', ATTRIBUTES)).toEqual([]);
    expect(parseWhereParam('{oops', ATTRIBUTES)).toEqual([]);
    expect(parseWhereParam('[1,2]', ATTRIBUTES)).toEqual([]);
  });
});
