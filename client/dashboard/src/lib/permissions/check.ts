import { z } from 'zod';
import { adminFetch, type AppAdminConfig } from '@/lib/users/admin';
import type { Rules } from './model';

/**
 * C.10 Test-as and C.12 Playground: the two admin debug endpoints and the
 * `$users` lookup behind the "Run as a user" combobox. Request bodies match
 * `@instantdb/admin`'s `debugQuery`/`debugTransact`; the server answers with
 * `check-results`, which this module normalizes into the rows the Checks table
 * renders. `admin` sends no impersonation header: the app's own token runs the
 * request, which is what the Playground's Admin mode uses for live queries.
 */

export type TestAsActor =
  | { kind: 'admin' }
  | { kind: 'guest' }
  | { kind: 'user'; email: string }
  | { kind: 'token'; token: string };

export interface PermissionsCheckRow {
  namespace: string;
  entityId: string | null;
  action: string;
  expression: string;
  allowed: boolean;
}

export interface DebugQueryResult {
  result: unknown;
  checks: PermissionsCheckRow[];
}

export interface DebugTransactResult {
  /** The full server body, for the Playground's response tab. */
  response: unknown;
  /** Whether the server committed when every check passed. */
  committed: boolean;
  allChecksOk: boolean;
  checks: PermissionsCheckRow[];
}

/** `as-guest`, `as-email` and `as-token` are sent next to the admin bearer. */
export function actorHeaders(actor: TestAsActor): Record<string, string> {
  switch (actor.kind) {
    case 'admin':
      return {};
    case 'guest':
      return { 'as-guest': 'true' };
    case 'user':
      return { 'as-email': actor.email.trim() };
    case 'token':
      return { 'as-token': actor.token.trim() };
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function text(value: unknown): string | undefined {
  return typeof value === 'string' && value.trim() !== '' ? value : undefined;
}

function programOf(check: Record<string, unknown>): Record<string, unknown> {
  return isRecord(check['program']) ? check['program'] : {};
}

function expressionOf(check: Record<string, unknown>): string {
  const program = programOf(check);
  return text(program['display-code']) ?? text(program['code']) ?? '';
}

function actionOf(check: Record<string, unknown>): string {
  return text(programOf(check)['action']) ?? text(check['action']) ?? 'view';
}

/**
 * Normalizes either server shape. Query rows carry `{entity,id,program,check}`;
 * transaction rows carry `{etype,eid,action,program,check-pass?}`.
 */
export function permissionsChecksFrom(
  raw: unknown,
  kind: 'query' | 'transact',
): PermissionsCheckRow[] {
  if (!Array.isArray(raw)) return [];
  const rows: PermissionsCheckRow[] = [];
  for (const entry of raw) {
    if (!isRecord(entry)) continue;
    const namespace =
      text(entry['entity']) ?? text(entry['etype']) ?? 'unknown';
    const entityId = text(entry['id']) ?? text(entry['eid']) ?? null;
    const allowed =
      kind === 'query'
        ? entry['check'] === true
        : entry['check-pass?'] === true || entry['check-result'] === true;
    rows.push({
      namespace,
      entityId,
      action: actionOf(entry),
      expression: expressionOf(entry),
      allowed,
    });
  }
  return rows;
}

const debugQueryResponseSchema = z.looseObject({
  result: z.unknown(),
  'check-results': z.unknown().optional(),
});

const debugTransactResponseSchema = z.looseObject({
  'tx-id': z.number().optional(),
  'all-checks-ok?': z.boolean().optional(),
  'committed?': z.boolean().optional(),
  'check-results': z.unknown().optional(),
});

export interface RunDebugQueryOptions {
  query: Record<string, unknown>;
  /** `useDraftRules` sends the editor draft as `rules-override`. */
  rules?: Rules;
  actor: TestAsActor;
}

/** `POST /admin/query_perms_check`, as `asUser(...).debugQuery()` sends it. */
export async function runDebugQuery(
  config: AppAdminConfig,
  { query, rules, actor }: RunDebugQueryOptions,
): Promise<DebugQueryResult> {
  const response = await adminFetch(
    config,
    '/admin/query_perms_check',
    {
      method: 'POST',
      headers: actorHeaders(actor),
      body: {
        query,
        'rules-override': rules,
        'inference?': false,
      },
    },
    debugQueryResponseSchema,
  );
  return {
    result: response.result,
    checks: permissionsChecksFrom(response['check-results'], 'query'),
  };
}

export interface RunDebugTransactOptions {
  steps: readonly unknown[];
  rules?: Rules;
  actor: TestAsActor;
  /** Off is a dry run; on commits when every rule allows the write. */
  commit: boolean;
}

/** `POST /admin/transact_perms_check`, as `debugTransact()` sends it. */
export async function runDebugTransact(
  config: AppAdminConfig,
  { steps, rules, actor, commit }: RunDebugTransactOptions,
): Promise<DebugTransactResult> {
  const response = await adminFetch(
    config,
    '/admin/transact_perms_check',
    {
      method: 'POST',
      headers: actorHeaders(actor),
      body: {
        steps,
        'rules-override': rules,
        'dangerously-commit-tx': commit,
      },
    },
    debugTransactResponseSchema,
  );
  return {
    response,
    committed: response['committed?'] === true,
    allChecksOk: response['all-checks-ok?'] === true,
    checks: permissionsChecksFrom(response['check-results'], 'transact'),
  };
}

const userSearchResponseSchema = z.looseObject({
  data: z.looseObject({
    $users: z.array(
      z.looseObject({
        id: z.string(),
        email: z.string().nullable().optional(),
      }),
    ),
  }),
});

export interface TestAsUserOption {
  id: string;
  email: string;
}

/** The `$users` combobox options: case-insensitive email search (C.10). */
export async function searchTestAsUsers(
  config: AppAdminConfig,
  search: string,
  limit = 20,
): Promise<TestAsUserOption[]> {
  const text = search.trim();
  const query = {
    $users: {
      $: {
        limit,
        order: { serverCreatedAt: 'desc' },
        ...(text === '' ? {} : { where: { email: { $ilike: `%${text}%` } } }),
      },
    },
  };
  const response = await adminFetch(
    config,
    '/admin/query',
    {
      method: 'POST',
      body: { query, 'inference?': false },
    },
    userSearchResponseSchema,
  );
  return response.data.$users.flatMap((user) =>
    user.email === null || user.email === undefined
      ? []
      : [{ id: user.id, email: user.email }],
  );
}

/** A useful starting query for the editor, based on the app's namespaces. */
export function defaultTestQuery(namespaces: readonly string[]): string {
  const namespace =
    namespaces.find((name) => !name.startsWith('$') && name !== '$default') ??
    '$users';
  return JSON.stringify({ [namespace]: {} }, null, 2);
}
