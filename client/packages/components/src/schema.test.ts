import { expect, test } from 'vitest';
import { dbAttrsToExplorerSchema } from './schema';

test('creates a namespace for a reverse identity without a forward attribute', () => {
  const namespaces = dbAttrsToExplorerSchema({
    author: {
      id: 'author',
      'forward-identity': ['author-fwd', 'posts', 'author'],
      'reverse-identity': ['author-rev', 'users', 'posts'],
      'index?': true,
      'unique?': false,
      cardinality: 'one',
      'value-type': 'ref',
    },
  });

  expect(namespaces).toEqual([
    expect.objectContaining({ name: 'posts' }),
    expect.objectContaining({
      name: 'users',
      attrs: [
        expect.objectContaining({
          isForward: false,
          name: 'posts',
          namespace: 'users',
        }),
      ],
    }),
  ]);
});

test('does not create a namespace for a disabled reverse identity', () => {
  const namespaces = dbAttrsToExplorerSchema({
    author: {
      id: 'author',
      'forward-identity': ['author-fwd', 'posts', 'author'],
      'reverse-identity': ['author-rev', 'users', 'posts', false],
      'index?': true,
      'unique?': false,
      cardinality: 'one',
      'value-type': 'ref',
    },
  });

  expect(namespaces).toEqual([expect.objectContaining({ name: 'posts' })]);
});
