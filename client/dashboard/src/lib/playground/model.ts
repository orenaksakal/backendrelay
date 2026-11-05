import {
  lineColumnAt,
  lintDocument,
  type EditorIssue,
} from '@/components/code-editor/lint';
import { formatCount } from '@/lib/format';
import type { TestAsActor } from '@/lib/permissions/check';
import type { PlaygroundActorKind } from './url-state';

/**
 * C.12 Playground model: parse the two editors, normalize debug responses and
 * build the status line. The transact editor accepts the same step tuples the
 * Admin SDK compiles `db.tx.*` chunks into (`[action, namespace, id, args?]`),
 * so a step copied from an SDK call keeps its exact server meaning.
 */

const STEP_ACTIONS = new Set([
  'create',
  'update',
  'merge',
  'delete',
  'link',
  'unlink',
  'ruleParams',
]);

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function isLookupRef(value: unknown): value is [string, unknown] {
  return (
    Array.isArray(value) &&
    value.length === 2 &&
    typeof value[0] === 'string' &&
    value[0] !== ''
  );
}

function issueAtStart(message: string, text: string): EditorIssue {
  const { line, column } = lineColumnAt(text, 0);
  return {
    from: 0,
    to: Math.min(1, text.length),
    line,
    column,
    message,
    severity: 'error',
  };
}

/** A useful starting query for the first user namespace, else `$users`. */
export function defaultPlaygroundQuery(namespaces: readonly string[]): string {
  const namespace =
    namespaces.find((name) => !name.startsWith('$') && name !== '$default') ??
    namespaces[0] ??
    '$users';
  return JSON.stringify({ [namespace]: { $: { limit: 10 } } }, null, 2);
}

export function parseQueryDocument(text: string): Record<string, unknown> {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text) as unknown;
  } catch {
    throw new Error('The query must be valid JSON.');
  }
  if (!isRecord(parsed)) {
    throw new Error('The query must be a JSON object.');
  }
  return parsed;
}

/** InstaQL editor validation: JSON syntax plus the object the server expects. */
export function lintQueryDocument(text: string): EditorIssue[] {
  const issues = lintDocument('instaql', text);
  if (issues.length > 0 || text.trim() === '') return issues;
  try {
    parseQueryDocument(text);
    return [];
  } catch (error) {
    const message =
      error instanceof Error
        ? error.message
        : 'The query must be a JSON object.';
    return [issueAtStart(message, text)];
  }
}

/** One-line problems for a step list, without pointing at an offset. */
export function transactionStepProblems(steps: unknown[]): string[] {
  if (steps.length === 0) return ['Add at least one transaction step.'];
  const problems: string[] = [];
  steps.forEach((step, index) => {
    const position = index + 1;
    if (!Array.isArray(step) || step.length < 3) {
      problems.push(
        `Step ${String(position)} must be ["action", "namespace", "id", args?].`,
      );
      return;
    }
    const [action, namespace, id, args] = step as unknown[];
    if (typeof action !== 'string' || !STEP_ACTIONS.has(action)) {
      problems.push(
        `Step ${String(position)} uses an unknown action. Use create, update, merge, delete, link, unlink or ruleParams.`,
      );
    }
    if (typeof namespace !== 'string' || namespace.trim() === '') {
      problems.push(`Step ${String(position)} needs a namespace.`);
    }
    const idOk =
      (typeof id === 'string' && id.trim() !== '') || isLookupRef(id);
    if (!idOk) {
      problems.push(
        `Step ${String(position)} needs an id or a ["attribute", value] lookup.`,
      );
    }
    if (
      action !== 'delete' &&
      action !== 'ruleParams' &&
      (args === undefined || step.length < 4)
    ) {
      problems.push(`Step ${String(position)} needs its arguments object.`);
    }
  });
  return problems;
}

export function parseTransactionSteps(text: string): unknown[] {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text) as unknown;
  } catch {
    throw new Error('The transaction steps must be valid JSON.');
  }
  if (!Array.isArray(parsed)) {
    throw new Error('The transaction steps must be a JSON array.');
  }
  const problem = transactionStepProblems(parsed)[0];
  if (problem !== undefined) throw new Error(problem);
  return parsed;
}

/** JSON syntax first, then the tuple checks the server would reject. */
export function lintTransactionSteps(text: string): EditorIssue[] {
  const issues = lintDocument('json', text);
  if (issues.length > 0 || text.trim() === '') return issues;

  let parsed: unknown;
  try {
    parsed = JSON.parse(text) as unknown;
  } catch {
    return issues;
  }
  if (!Array.isArray(parsed)) {
    return [issueAtStart('The transaction steps must be a JSON array.', text)];
  }
  return transactionStepProblems(parsed).map((message) =>
    issueAtStart(message, text),
  );
}

/** The namespace payload of a debug or live result. */
export function queryData(result: unknown): Record<string, unknown> {
  if (!isRecord(result)) return {};
  const data = result['data'];
  return isRecord(data) ? data : result;
}

export interface QueryRows {
  namespace: string;
  rows: ReadonlyArray<Record<string, unknown>>;
  /** `id` first, then every other attribute in first-seen order. */
  columns: readonly string[];
}

/** The rows of the first namespace in the result, for the Table tab. */
export function firstNamespaceRows(result: unknown): QueryRows | null {
  const data = queryData(result);
  for (const [namespace, value] of Object.entries(data)) {
    if (!Array.isArray(value)) continue;
    const rows = value.filter(isRecord);
    const seen = new Set<string>();
    for (const row of rows) {
      for (const key of Object.keys(row)) seen.add(key);
    }
    const columns = [
      ...(seen.has('id') ? ['id'] : []),
      ...[...seen].filter((key) => key !== 'id').sort(),
    ];
    return { namespace, rows, columns };
  }
  return null;
}

/** C.12 status: "{n} entities · {ms} ms". */
export function countEntities(result: unknown): number {
  return Object.values(queryData(result)).reduce<number>(
    (total, value) => total + (Array.isArray(value) ? value.length : 0),
    0,
  );
}

export function entityLabel(count: number): string {
  return `${formatCount(count)} ${count === 1 ? 'entity' : 'entities'}`;
}

export function queryStatus(count: number, durationMs: number): string {
  return `${entityLabel(count)} · ${String(Math.round(durationMs))} ms`;
}

/** The message of a live-query error result, or `null` when it succeeded. */
export function liveQueryError(result: unknown): string | null {
  if (!isRecord(result)) return null;
  const error = result['error'];
  if (!isRecord(error)) return null;
  const message = error['message'];
  return typeof message === 'string' && message.trim() !== ''
    ? message
    : 'The query failed.';
}

/** Validates the "Run as" controls and turns them into request headers. */
export function playgroundActor(
  kind: PlaygroundActorKind,
  email: string,
  token: string,
): TestAsActor | string {
  switch (kind) {
    case 'admin':
      return { kind: 'admin' };
    case 'guest':
      return { kind: 'guest' };
    case 'email': {
      const value = email.trim();
      return value === ''
        ? 'Choose a user, or enter their email.'
        : { kind: 'user', email: value };
    }
    case 'token': {
      const value = token.trim();
      return value === ''
        ? 'Paste a refresh token, or run as a guest.'
        : { kind: 'token', token: value };
    }
  }
}

/** The label the History menu and a saved run show for the actor. */
export function actorLabel(kind: PlaygroundActorKind, email: string): string {
  if (kind === 'email') {
    const value = email.trim();
    return value === '' ? 'User email' : value;
  }
  if (kind === 'token') return 'Token';
  if (kind === 'guest') return 'Guest';
  return 'Admin';
}

/** A one-line preview of an editor document for the History menu. */
export function sourceSummary(source: string): string {
  const lines = source
    .split('\n')
    .map((line) => line.trim())
    .filter((line) => line !== '');
  const firstLine =
    lines.find((line) => line !== '{' && line !== '[') ?? lines[0];
  if (firstLine === undefined) return 'Empty document';
  return firstLine.length > 48 ? `${firstLine.slice(0, 47)}…` : firstLine;
}
