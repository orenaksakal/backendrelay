import type { InstaqlWhere } from '@/components/data/filter-serialization';
import { linkedEntities, type LinkedEntity } from '@/lib/explorer/entities';
import type { AppLink } from '@/lib/instant';
import type { UsersType } from './url-state';

/**
 * The `$users` list for C.8: one page at a time, newest first (B.6). The
 * filters are the two the screen offers — a case-insensitive email search and
 * the All / With email / Guest type filter.
 */

export const USERS_NAMESPACE = '$users';

/** C.8 pages through users 50 at a time with "Load more". */
export const USERS_PAGE_SIZE = 50;

/** Guards "Load more" against an unbounded single request. */
export const MAX_USERS_PAGE_SIZE = 10_000;

export const USER_CREATED_AT_ATTRIBUTE = 'createdAt';

/** A `$users` entity. Only `id` is guaranteed; guests have no email. */
export type UserRow = { id: string } & Record<string, unknown>;

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function isUnknownArray(value: unknown): value is unknown[] {
  return Array.isArray(value);
}

export function isUserRow(value: unknown): value is UserRow {
  return isRecord(value) && typeof value['id'] === 'string';
}

/**
 * InstaQL `where` for the toolbar. `$isNull` covers both a missing and an
 * explicit `null` email, so guests stay visible after an upgrade removes one.
 * Search and type never share the `email` key: InstaQL reads only the first
 * operator on an attribute, and a matching email is already non-null.
 */
export function buildUsersWhere({
  text,
  type,
}: {
  text: string;
  type: UsersType;
}): InstaqlWhere {
  const search = text.trim();
  if (type === 'guest') return { email: { $isNull: true } };
  if (search !== '') return { email: { $ilike: `%${search}%` } };
  if (type === 'email') return { email: { $isNull: false } };
  return {};
}

/** `{ $users: { $: { where, limit, offset, order } } }` (C.8). */
export function buildUsersQuery({
  where,
  limit,
  offset = 0,
}: {
  where: InstaqlWhere;
  limit: number;
  offset?: number;
}): Record<string, unknown> {
  const options: Record<string, unknown> = {
    limit,
    offset,
    order: { serverCreatedAt: 'desc' },
  };
  if (Object.keys(where).length > 0) options['where'] = where;
  return { [USERS_NAMESPACE]: { $: options } };
}

/** One user plus its linked data, subscribed only while the sheet is open. */
export function buildUserQuery(
  userId: string,
  linkLabels: ReadonlyArray<string>,
): Record<string, unknown> {
  const form: Record<string, unknown> = {};
  for (const label of linkLabels) form[label] = {};
  form['$'] = { where: { id: userId }, limit: 1 };
  return { [USERS_NAMESPACE]: form };
}

export interface UsersQueryResult {
  users: UserRow[];
  hasNextPage: boolean;
}

/** The page of users and the next-page flag from a `subscribeQuery` result. */
export function parseUsersResult(result: unknown): UsersQueryResult {
  const payload: Record<string, unknown> = isRecord(result) ? result : {};
  const data = isRecord(payload['data'])
    ? payload['data'][USERS_NAMESPACE]
    : undefined;
  const users = isUnknownArray(data)
    ? data.filter((row): row is UserRow => isUserRow(row))
    : [];

  const pageInfo = isRecord(payload['pageInfo'])
    ? payload['pageInfo'][USERS_NAMESPACE]
    : undefined;
  const hasNextPage = isRecord(pageInfo) && pageInfo['hasNextPage'] === true;

  return { users, hasNextPage };
}

/** A query failure message, or `null` for a successful result. */
export function parseUsersError(result: unknown): string | null {
  if (!isRecord(result)) return null;
  const error = result['error'];
  if (!isRecord(error)) return null;
  const message = error['message'];
  return typeof message === 'string' && message !== ''
    ? message
    : 'The query failed.';
}

/** The address a user signed in with, or `null` for guests. */
export function userEmail(user: UserRow): string | null {
  const email = user['email'];
  if (typeof email !== 'string') return null;
  const trimmed = email.trim();
  return trimmed === '' ? null : trimmed;
}

/** C.8 treats a user without an email as a guest. */
export function isGuestUser(user: UserRow): boolean {
  return userEmail(user) === null;
}

/** The stored `type` when the server set one, else inferred from the email. */
export function userTypeLabel(user: UserRow): string {
  const type = user['type'];
  if (typeof type === 'string' && type.trim() !== '') return type;
  return isGuestUser(user) ? 'guest' : 'user';
}

/** `createdAt` as RelativeTime understands it; `null` renders an em dash. */
export function userCreatedAt(user: UserRow): string | number | null {
  const createdAt = user[USER_CREATED_AT_ATTRIBUTE];
  if (typeof createdAt === 'string' || typeof createdAt === 'number') {
    return createdAt;
  }
  return null;
}

export interface UserLinkGroup {
  link: AppLink;
  entities: LinkedEntity[];
}

/** Linked data for the sheet, one group per link that starts at `$users`. */
export function userLinkGroups(
  user: UserRow | null,
  links: ReadonlyArray<AppLink>,
): UserLinkGroup[] {
  if (user === null) return [];
  return links.map((link) => ({
    link,
    entities: linkedEntities(user[link.label]),
  }));
}
