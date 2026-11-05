import assert from 'node:assert/strict';
import test from 'node:test';

import { CANARY_ROW_LIMIT, rowsToPrune } from '../src/cleanup.mjs';

test('keeps the newest rows and prunes the rest', () => {
  const rows = Array.from({ length: 5 }, (_, index) => ({
    id: `row-${index}`,
    at: 1_000 + index,
  }));
  assert.deepEqual(rowsToPrune(rows, 3), ['row-1', 'row-0']);
  assert.deepEqual(rowsToPrune(rows.slice(0, 3), 3), []);
});

test('prunes ties deterministically and ignores malformed rows', () => {
  const rows = [
    { id: 'b', at: 10 },
    { id: 'a', at: 10 },
    { id: '', at: 20 },
    { id: 'c' },
    null,
    { at: 30 },
    'nope',
  ];
  assert.deepEqual(rowsToPrune(rows, 2), ['c']);
});

test('defaults to the documented row limit', () => {
  const rows = Array.from({ length: CANARY_ROW_LIMIT + 5 }, (_, index) => ({
    id: `row-${index}`,
    at: index,
  }));
  const pruned = rowsToPrune(rows);
  assert.equal(pruned.length, 5);
  assert.deepEqual(pruned, ['row-4', 'row-3', 'row-2', 'row-1', 'row-0']);
});
