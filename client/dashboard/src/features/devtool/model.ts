import { formatCount } from '@/lib/format';

/**
 * C.20 SDK devtool helpers. The page is embedded in an iframe by the client
 * SDK (`client/packages/core/src/devtool.ts`), so the close protocol and the
 * compact query playground live here instead of inside the router.
 */

/** The message the SDK listens for to hide its iframe. */
export const DEVTOOL_CLOSE_MESSAGE = { type: 'close' } as const;

/** Posts the close message to the embedding page. */
export function closeDevtool(
  target: Pick<Window, 'postMessage'> = window.parent,
): void {
  target.postMessage(DEVTOOL_CLOSE_MESSAGE, '*');
}

export interface PlaygroundNamespace {
  name: string;
  isSystem: boolean;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

/**
 * The starting InstaQL document: the first user namespace, so the Playground
 * opens with something that returns rows.
 */
export function defaultInstaqlQuery(
  namespaces: ReadonlyArray<PlaygroundNamespace>,
): string {
  const target =
    namespaces.find((namespace) => !namespace.isSystem) ?? namespaces[0];
  if (target === undefined) return '{}';
  return JSON.stringify({ [target.name]: {} }, null, 2);
}

export type ParsedInstaqlQuery =
  | { ok: true; query: Record<string, unknown> }
  | { ok: false; message: string };

/** Parses the editor document; the server remains the query authority. */
export function parseInstaqlQuery(text: string): ParsedInstaqlQuery {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text) as unknown;
  } catch {
    return { ok: false, message: 'The query must be valid JSON.' };
  }

  if (!isRecord(parsed)) {
    return {
      ok: false,
      message: 'The query must be a JSON object, for example { "todos": {} }.',
    };
  }
  if (Object.keys(parsed).length === 0) {
    return { ok: false, message: 'Enter at least one namespace to query.' };
  }
  return { ok: true, query: parsed };
}

/** True once the callback carries a data payload, not just a loading state. */
export function queryResultHasData(result: unknown): boolean {
  return isRecord(result) && isRecord(result['data']);
}

/** Entity count across every namespace in a query result. */
export function queryResultCount(result: unknown): number {
  if (!isRecord(result)) return 0;
  const data = result['data'];
  if (!isRecord(data)) return 0;

  let total = 0;
  for (const value of Object.values(data)) {
    if (Array.isArray(value)) total += value.length;
  }
  return total;
}

/** The server's query error message, or `null` when the run succeeded. */
export function queryResultError(result: unknown): string | null {
  if (!isRecord(result)) return null;
  const error = result['error'];
  if (!isRecord(error)) return null;
  const message = error['message'];
  return typeof message === 'string' && message !== ''
    ? message
    : 'The query failed.';
}

/** `2 entities · 14 ms` for the Playground status line. */
export function playgroundStatus(count: number, ms: number): string {
  return `${formatCount(count)} ${count === 1 ? 'entity' : 'entities'} · ${String(ms)} ms`;
}

export interface PlaygroundTable {
  namespace: string;
  columns: string[];
  rows: Array<Record<string, unknown>>;
}

/** The Table result view: the first namespace, with its row keys as columns. */
export function playgroundTable(result: unknown): PlaygroundTable | null {
  if (!isRecord(result)) return null;
  const data = result['data'];
  if (!isRecord(data)) return null;

  for (const [namespace, value] of Object.entries(data)) {
    if (!Array.isArray(value)) continue;
    const rows = value.filter(isRecord);
    const columns: string[] = [];
    for (const row of rows) {
      for (const key of Object.keys(row)) {
        if (!columns.includes(key)) columns.push(key);
      }
    }
    if (columns.includes('id')) {
      columns.splice(columns.indexOf('id'), 1);
      columns.unshift('id');
    }
    return { namespace, columns, rows };
  }
  return null;
}

/** Cell text for the Table result view. */
export function playgroundCell(value: unknown): string {
  if (value === undefined) return 'unset';
  if (value === null) return 'null';
  if (typeof value === 'string') return value;
  if (typeof value === 'number' || typeof value === 'boolean') {
    return String(value);
  }
  return JSON.stringify(value);
}
