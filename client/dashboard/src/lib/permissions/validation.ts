/**
 * D0.5 returns validation errors with a path into the rules document, e.g.
 * `["todos", "allow", "view"]`. The visual editor maps those paths onto the row
 * that owns them so the server message appears next to the expression.
 */

import {
  actionRowKey,
  bindRowKey,
  fieldRowKey,
  relationshipRowKey,
} from './document';
import { PERMISSION_ACTIONS, type PermissionAction } from './effective';

export interface ServerRuleError {
  in: readonly string[];
  message: string;
}

export type DraftValidation =
  | { state: 'idle' }
  | { state: 'checking' }
  | { state: 'valid' }
  | { state: 'invalid'; errors: readonly ServerRuleError[] };

export interface RowErrors {
  byRow: Map<string, string[]>;
  /** Errors the visual editor has no row for; the page lists them above. */
  unattached: ServerRuleError[];
}

function isPermissionAction(value: string): value is PermissionAction {
  return (PERMISSION_ACTIONS as readonly string[]).includes(value);
}

function pathRowKey(path: readonly string[]): string | null {
  const [namespace, group, name, label] = path;
  if (namespace === undefined || group === undefined) return null;
  if (group === 'allow' && name !== undefined) {
    if (name === 'link' || name === 'unlink') {
      if (label === undefined) return null;
      return relationshipRowKey(namespace, name, label);
    }
    if (isPermissionAction(name)) return actionRowKey(namespace, name);
    return null;
  }
  if (group === 'fields' && name !== undefined) {
    return fieldRowKey(namespace, name);
  }
  if (group === 'bind' && name !== undefined) {
    return bindRowKey(namespace, name);
  }
  return null;
}

export function rowErrors(errors: readonly ServerRuleError[]): RowErrors {
  const byRow = new Map<string, string[]>();
  const unattached: ServerRuleError[] = [];
  for (const error of errors) {
    const key = pathRowKey(error.in);
    if (key === null) {
      unattached.push(error);
      continue;
    }
    const messages = byRow.get(key) ?? [];
    messages.push(error.message);
    byRow.set(key, messages);
  }
  return { byRow, unattached };
}

/** Human-readable path for errors that do not belong to a row. */
export function errorPath(error: ServerRuleError): string {
  return error.in.length > 0 ? error.in.join(' → ') : 'Rules';
}
