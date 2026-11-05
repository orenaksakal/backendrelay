import { z } from 'zod';

/**
 * Users URL state (C.8, B.6): `q` searches emails, `type` narrows the list to
 * users with an email or to guests, and `user` keeps the open sheet in the URL
 * so a copied link restores it.
 */

export const USERS_TYPES = ['all', 'email', 'guest'] as const;

export type UsersType = (typeof USERS_TYPES)[number];

export const USERS_TYPE_LABELS: Record<UsersType, string> = {
  all: 'All',
  email: 'With email',
  guest: 'Guest',
};

export function isUsersType(value: unknown): value is UsersType {
  return USERS_TYPES.some((type) => type === value);
}

/** A hand-edited URL falls back to the default instead of failing the route. */
export const usersSearchSchema = z.object({
  q: z.string().trim().min(1).optional().catch(undefined),
  type: z.enum(USERS_TYPES).catch('all'),
  user: z.string().trim().min(1).optional().catch(undefined),
});

export type UsersSearch = z.infer<typeof usersSearchSchema>;

/** The URL form of a view state; empty values are dropped from the URL. */
export function usersSearchFor(state: {
  q?: string | undefined;
  type: UsersType;
  user?: string | null | undefined;
}): UsersSearch {
  const text = state.q?.trim() ?? '';
  return {
    q: text === '' ? undefined : text,
    type: state.type,
    user: state.user ?? undefined,
  };
}
