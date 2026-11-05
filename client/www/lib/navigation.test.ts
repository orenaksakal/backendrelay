import { expect, test } from 'vitest';
import {
  hrefToString,
  isStartOwnedHref,
  isStartOwnedPathname,
  queryFromSearch,
  shouldUseShallowHistory,
} from './navigation';

test('serializes query arrays and keeps an object href on the current path', () => {
  expect(
    hrefToString(
      {
        query: { tag: ['one', 'two'], empty: '', omitted: undefined },
        hash: '#results',
      },
      '/docs',
    ),
  ).toBe('/docs?tag=one&tag=two&empty=#results');
});

test('classifies only registered same-origin Start routes', () => {
  expect(isStartOwnedPathname('/dash')).toBe(true);
  expect(isStartOwnedPathname('/intern/deployment-settings')).toBe(true);
  expect(isStartOwnedPathname('/docs')).toBe(false);
  expect(isStartOwnedHref('/dash?app=1', 'https://instantdb.com')).toBe(true);
  expect(isStartOwnedHref('/docs?tab=react', 'https://instantdb.com')).toBe(
    false,
  );
  expect(
    isStartOwnedHref('https://example.com/dash', 'https://instantdb.com'),
  ).toBe(false);
  expect(isStartOwnedHref('http://[', 'https://instantdb.com')).toBe(false);
});

test('uses history only for shallow same-path public URL updates', () => {
  expect(
    shouldUseShallowHistory('/docs?tab=react', 'https://instantdb.com/docs', {
      shallow: true,
    }),
  ).toBe(true);
  expect(
    shouldUseShallowHistory('/docs/intro', 'https://instantdb.com/docs', {
      shallow: true,
    }),
  ).toBe(false);
  expect(
    shouldUseShallowHistory('/docs?tab=react', 'https://instantdb.com/docs'),
  ).toBe(false);
});

test('merges route params into legacy router query names', () => {
  expect(
    queryFromSearch('?tag=one&tag=two', { traceId: 'trace', slug: 'post' }),
  ).toEqual({
    tag: ['one', 'two'],
    traceId: 'trace',
    'trace-id': 'trace',
    slug: 'post',
  });
});
