import { expect, test } from 'vitest';
import { dbAttrsToExplorerSchema } from './schema';

test('adds a namespace referenced only by a reverse identity', () => {
  const namespaces = dbAttrsToExplorerSchema({
    postsAuthor: {
      id: 'postsAuthor',
      'forward-identity': ['postsAuthor', 'posts', 'author'],
      'reverse-identity': ['postsAuthor', 'users', 'posts'],
      'value-type': 'ref',
      cardinality: 'one',
      'index?': false,
      'unique?': false,
    },
  });

  const users = namespaces.find((namespace) => namespace.name === 'users');
  const posts = namespaces.find((namespace) => namespace.name === 'posts');

  expect(users?.attrs).toHaveLength(1);
  expect(users?.attrs[0]).toMatchObject({
    isForward: false,
    namespace: 'users',
    name: 'posts',
  });
  expect(users?.attrs[0].linkConfig.reverse?.nsMap).toMatchObject({
    id: 'users',
    name: 'users',
  });
  expect(posts?.attrs[0].linkConfig.reverse?.nsMap).toMatchObject({
    id: 'users',
    name: 'users',
  });
});
