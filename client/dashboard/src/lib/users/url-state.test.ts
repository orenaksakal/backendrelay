import { describe, expect, it } from 'vitest';
import { isUsersType, usersSearchFor, usersSearchSchema } from './url-state';

describe('users URL state', () => {
  it('defaults to every user with no search or sheet', () => {
    const search = usersSearchSchema.parse({});
    expect(search.type).toBe('all');
    expect(search.q).toBeUndefined();
    expect(search.user).toBeUndefined();
  });

  it('falls back to All for an unknown type', () => {
    expect(usersSearchSchema.parse({ type: 'admins' }).type).toBe('all');
  });

  it('trims the search and drops an empty one', () => {
    expect(usersSearchSchema.parse({ q: '  ada  ' }).q).toBe('ada');
    expect(usersSearchSchema.parse({ q: '   ' }).q).toBeUndefined();
    expect(usersSearchSchema.parse({ user: ' user-1 ' }).user).toBe('user-1');
  });

  it('writes compact search params', () => {
    expect(
      usersSearchFor({ q: ' ada@example.com ', type: 'email', user: 'user-1' }),
    ).toEqual({ q: 'ada@example.com', type: 'email', user: 'user-1' });
    expect(usersSearchFor({ q: '  ', type: 'guest', user: null })).toEqual({
      q: undefined,
      type: 'guest',
      user: undefined,
    });
  });

  it('guards the type values the select offers', () => {
    expect(isUsersType('guest')).toBe(true);
    expect(isUsersType('all')).toBe(true);
    expect(isUsersType('email')).toBe(true);
    expect(isUsersType('admins')).toBe(false);
  });
});
