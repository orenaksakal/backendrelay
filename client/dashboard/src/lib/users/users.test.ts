import { describe, expect, it } from 'vitest';
import type { AppLink } from '@/lib/instant';
import {
  buildUserQuery,
  buildUsersQuery,
  buildUsersWhere,
  isGuestUser,
  isUserRow,
  parseUsersError,
  parseUsersResult,
  userCreatedAt,
  userEmail,
  userLinkGroups,
  userTypeLabel,
} from './users';

const ownerLink: AppLink = {
  id: 'attr-owner',
  namespace: '$users',
  label: 'todos',
  targetNamespace: 'todos',
  reverseLabel: 'owner',
  cardinality: 'many',
  onDelete: undefined,
};

describe('users where', () => {
  it('is empty for the unfiltered list', () => {
    expect(buildUsersWhere({ text: '', type: 'all' })).toEqual({});
    expect(buildUsersWhere({ text: '   ', type: 'all' })).toEqual({});
  });

  it('searches emails case-insensitively', () => {
    expect(buildUsersWhere({ text: ' Ada ', type: 'email' })).toEqual({
      email: { $ilike: '%Ada%' },
    });
    expect(buildUsersWhere({ text: 'ada', type: 'all' })).toEqual({
      email: { $ilike: '%ada%' },
    });
  });

  it('filters users without an email as guests', () => {
    expect(buildUsersWhere({ text: '', type: 'guest' })).toEqual({
      email: { $isNull: true },
    });
    // A guest has no email to search, so the text filter is not combined with
    // the `$isNull` operator.
    expect(buildUsersWhere({ text: 'ada', type: 'guest' })).toEqual({
      email: { $isNull: true },
    });
  });

  it('keeps only users with an email when asked', () => {
    expect(buildUsersWhere({ text: '', type: 'email' })).toEqual({
      email: { $isNull: false },
    });
  });
});

describe('users query', () => {
  it('orders newest first with the C.8 page size', () => {
    expect(buildUsersQuery({ where: {}, limit: 50 })).toEqual({
      $users: {
        $: { limit: 50, offset: 0, order: { serverCreatedAt: 'desc' } },
      },
    });
  });

  it('adds the where clause and an offset when asked', () => {
    expect(
      buildUsersQuery({
        where: { email: { $isNull: true } },
        limit: 100,
        offset: 50,
      }),
    ).toEqual({
      $users: {
        $: {
          where: { email: { $isNull: true } },
          limit: 100,
          offset: 50,
          order: { serverCreatedAt: 'desc' },
        },
      },
    });
  });

  it('fetches linked data only for the open sheet', () => {
    expect(buildUserQuery('user-1', ['todos'])).toEqual({
      $users: {
        todos: {},
        $: { where: { id: 'user-1' }, limit: 1 },
      },
    });
    expect(buildUserQuery('user-1', [])).toEqual({
      $users: { $: { where: { id: 'user-1' }, limit: 1 } },
    });
  });
});

describe('users results', () => {
  it('reads the page and the next-page flag', () => {
    const result = parseUsersResult({
      data: {
        $users: [
          { id: 'user-1', email: 'ada@example.com', type: 'user' },
          { id: 'user-2' },
          { email: 'no-id@example.com' },
        ],
      },
      pageInfo: { $users: { hasNextPage: true } },
    });

    expect(result.users.map((user) => user.id)).toEqual(['user-1', 'user-2']);
    expect(result.hasNextPage).toBe(true);
  });

  it('treats a loading or malformed result as an empty page', () => {
    expect(parseUsersResult(undefined)).toEqual({
      users: [],
      hasNextPage: false,
    });
    expect(parseUsersResult({ data: { $users: null } })).toEqual({
      users: [],
      hasNextPage: false,
    });
  });

  it('surfaces query errors', () => {
    expect(parseUsersError({ error: { message: 'Permission denied' } })).toBe(
      'Permission denied',
    );
    expect(parseUsersError({ error: {} })).toBe('The query failed.');
    expect(parseUsersError({ data: { $users: [] } })).toBeNull();
  });
});

describe('user fields', () => {
  it('recognises rows and guest users', () => {
    expect(isUserRow({ id: 'user-1' })).toBe(true);
    expect(isUserRow({ email: 'ada@example.com' })).toBe(false);
    expect(isGuestUser({ id: 'user-1' })).toBe(true);
    expect(isGuestUser({ id: 'user-1', email: '   ' })).toBe(true);
    expect(isGuestUser({ id: 'user-1', email: 'ada@example.com' })).toBe(false);
  });

  it('normalises the email, type and created date', () => {
    const user = {
      id: 'user-1',
      email: '  ada@example.com  ',
      type: 'user',
      createdAt: '2026-10-01T00:00:00Z',
    };
    expect(userEmail(user)).toBe('ada@example.com');
    expect(userTypeLabel(user)).toBe('user');
    expect(userCreatedAt(user)).toBe('2026-10-01T00:00:00Z');
    expect(userTypeLabel({ id: 'user-2' })).toBe('guest');
    expect(userCreatedAt({ id: 'user-2' })).toBeNull();
  });
});

describe('user links', () => {
  it('groups linked entities by link label', () => {
    const groups = userLinkGroups(
      {
        id: 'user-1',
        todos: [{ id: 'todo-1', title: 'Buy milk' }],
      },
      [ownerLink],
    );

    expect(groups).toHaveLength(1);
    expect(groups[0]?.link.label).toBe('todos');
    expect(groups[0]?.entities).toEqual([{ id: 'todo-1', label: 'Buy milk' }]);
  });

  it('has nothing to show without a user', () => {
    expect(userLinkGroups(null, [ownerLink])).toEqual([]);
  });
});
