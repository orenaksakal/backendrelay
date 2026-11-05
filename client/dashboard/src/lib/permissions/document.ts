/**
 * C.10/D7.2 visual editor document operations. Everything is expressed as
 * copies of the rules object so the JSON and visual views share one draft and
 * a change never mutates the saved document. Empty containers are removed on
 * the way out, which is what "Reset to inherited" means for each rule level.
 */

import { PERMISSION_ACTIONS, type PermissionAction } from './effective';
import type { Rules } from './model';

export type RelationshipKind = 'link' | 'unlink';
export type BindShape = 'array' | 'object';

export interface BindRow {
  name: string;
  expression: string;
}

export interface Binds {
  shape: BindShape;
  rows: BindRow[];
  /** False when the stored shape is neither `["name", "expr", …]` nor `{name: expr}`. */
  valid: boolean;
}

export interface ExpressionRow {
  name: string;
  expression: string;
}

const KNOWN_NAMESPACE_KEYS = new Set(['allow', 'bind', 'fields']);
const KNOWN_ALLOW_KEYS = new Set([
  ...PERMISSION_ACTIONS,
  '$default',
  'link',
  'unlink',
]);
const RESERVED_TOP_LEVEL = new Set(['$rateLimits', 'attrs']);

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function cloneRules(rules: unknown): Rules {
  return JSON.parse(JSON.stringify(isRecord(rules) ? rules : {})) as Rules;
}

function ensureRecord(
  parent: Record<string, unknown>,
  key: string,
): Record<string, unknown> {
  const value = parent[key];
  if (isRecord(value)) return value;
  const created: Record<string, unknown> = {};
  parent[key] = created;
  return created;
}

function dropEmpty(parent: Record<string, unknown>, key: string): void {
  const value = parent[key];
  if (isRecord(value) && Object.keys(value).length === 0)
    Reflect.deleteProperty(parent, key);
  if (Array.isArray(value) && value.length === 0)
    Reflect.deleteProperty(parent, key);
}

function finish(document: Rules, namespace: string): void {
  const block = document[namespace];
  if (!isRecord(block)) return;
  dropEmpty(block, 'allow');
  dropEmpty(block, 'fields');
  dropEmpty(block, 'bind');
  if (Object.keys(block).length === 0)
    Reflect.deleteProperty(document, namespace);
}

export function namespaceBlock(
  rules: unknown,
  namespace: string,
): Record<string, unknown> | null {
  if (!isRecord(rules)) return null;
  const block = rules[namespace];
  return isRecord(block) ? block : null;
}

export function ownExpression(
  rules: unknown,
  namespace: string,
  action: PermissionAction,
): string | null {
  const block = namespaceBlock(rules, namespace);
  const allow = block?.['allow'];
  if (!isRecord(allow)) return null;
  const value = allow[action];
  if (typeof value === 'string') return value;
  if (typeof value === 'boolean') return String(value);
  return null;
}

export function setAllowExpression(
  rules: unknown,
  namespace: string,
  action: PermissionAction,
  expression: string | null,
): Rules {
  const document = cloneRules(rules);
  if (expression === null) {
    const block = document[namespace];
    if (!isRecord(block)) return document;
    const allow = block['allow'];
    if (!isRecord(allow)) return document;
    Reflect.deleteProperty(allow, action);
    finish(document, namespace);
    return document;
  }
  const block = ensureRecord(document, namespace);
  const allow = ensureRecord(block, 'allow');
  allow[action] = expression;
  finish(document, namespace);
  return document;
}

function readNestedExpressions(
  rules: unknown,
  namespace: string,
  parent: 'allow' | 'fields',
  key: string,
): ExpressionRow[] {
  const block = namespaceBlock(rules, namespace);
  const container = block?.[parent];
  if (!isRecord(container)) return [];
  const nested = parent === 'allow' ? container[key] : container;
  if (!isRecord(nested)) return [];
  return Object.entries(nested)
    .filter((entry): entry is [string, string] => typeof entry[1] === 'string')
    .map(([name, expression]) => ({ name, expression }))
    .sort((left, right) => left.name.localeCompare(right.name));
}

export function relationshipRows(
  rules: unknown,
  namespace: string,
  kind: RelationshipKind,
): ExpressionRow[] {
  return readNestedExpressions(rules, namespace, 'allow', kind);
}

export function fieldRows(rules: unknown, namespace: string): ExpressionRow[] {
  return readNestedExpressions(rules, namespace, 'fields', 'fields');
}

function setNestedExpression(
  rules: unknown,
  namespace: string,
  parent: 'allow' | 'fields',
  key: string,
  name: string,
  expression: string | null,
): Rules {
  const document = cloneRules(rules);
  const block = document[namespace];
  if (expression === null) {
    if (!isRecord(block)) return document;
    const container = block[parent];
    if (!isRecord(container)) return document;
    if (parent === 'allow') {
      const nested = container[key];
      if (isRecord(nested)) {
        Reflect.deleteProperty(nested, name);
        dropEmpty(container, key);
      }
    } else {
      Reflect.deleteProperty(container, name);
    }
    finish(document, namespace);
    return document;
  }

  const nextBlock = ensureRecord(document, namespace);
  const container = ensureRecord(nextBlock, parent);
  if (parent === 'allow') {
    const nested = ensureRecord(container, key);
    nested[name] = expression;
  } else {
    container[name] = expression;
  }
  finish(document, namespace);
  return document;
}

export function setRelationshipExpression(
  rules: unknown,
  namespace: string,
  kind: RelationshipKind,
  label: string,
  expression: string | null,
): Rules {
  return setNestedExpression(
    rules,
    namespace,
    'allow',
    kind,
    label,
    expression,
  );
}

export function setFieldExpression(
  rules: unknown,
  namespace: string,
  field: string,
  expression: string | null,
): Rules {
  return setNestedExpression(
    rules,
    namespace,
    'fields',
    'fields',
    field,
    expression,
  );
}

/** Reads either supported bind shape: `["name", "expr", …]` or `{name: expr}`. */
export function readBinds(rules: unknown, namespace: string): Binds {
  const block = namespaceBlock(rules, namespace);
  const raw = block?.['bind'];
  if (raw === undefined || raw === null) {
    return { shape: 'object', rows: [], valid: true };
  }
  if (Array.isArray(raw)) {
    const items = raw as unknown[];
    if (
      items.length % 2 !== 0 ||
      items.some((value) => typeof value !== 'string')
    ) {
      return { shape: 'array', rows: [], valid: false };
    }
    const rows: BindRow[] = [];
    for (let index = 0; index < items.length; index += 2) {
      const name = items[index];
      const expression = items[index + 1];
      if (typeof name !== 'string' || typeof expression !== 'string') {
        return { shape: 'array', rows: [], valid: false };
      }
      rows.push({ name, expression });
    }
    return { shape: 'array', rows, valid: true };
  }
  if (isRecord(raw)) {
    const rows: BindRow[] = [];
    for (const [name, expression] of Object.entries(raw)) {
      if (typeof expression !== 'string') {
        return { shape: 'object', rows: [], valid: false };
      }
      rows.push({ name, expression });
    }
    return { shape: 'object', rows, valid: true };
  }
  return { shape: 'object', rows: [], valid: false };
}

/** Existing documents keep their shape; new bind rows use the object form. */
export function setBinds(
  rules: unknown,
  namespace: string,
  rows: readonly BindRow[],
  shape: BindShape,
): Rules {
  const document = cloneRules(rules);
  const block = document[namespace];
  if (rows.length === 0) {
    if (isRecord(block)) delete block['bind'];
    finish(document, namespace);
    return document;
  }
  const nextBlock = ensureRecord(document, namespace);
  if (shape === 'array') {
    nextBlock['bind'] = rows.flatMap((row) => [row.name, row.expression]);
  } else {
    const object: Record<string, string> = {};
    for (const row of rows) object[row.name] = row.expression;
    nextBlock['bind'] = object;
  }
  finish(document, namespace);
  return document;
}

/** Adds an empty namespace block so the sidebar can open it. */
export function addNamespace(rules: unknown, namespace: string): Rules {
  const document = cloneRules(rules);
  if (!isRecord(document[namespace])) document[namespace] = {};
  return document;
}

export function isEditorNamespace(name: string): boolean {
  return !RESERVED_TOP_LEVEL.has(name);
}

/** `$default` first, then user namespaces, then system namespaces (C.10). */
export function editorNamespaces(
  rules: unknown,
  schemaNamespaces: readonly string[] = [],
): string[] {
  const names = new Set<string>();
  for (const name of schemaNamespaces) {
    if (isEditorNamespace(name)) names.add(name);
  }
  if (isRecord(rules)) {
    for (const name of Object.keys(rules)) {
      if (isEditorNamespace(name)) names.add(name);
    }
  }
  const rest = [...names]
    .filter((name) => name !== '$default')
    .sort((left, right) => left.localeCompare(right));
  const user = rest.filter((name) => !name.startsWith('$'));
  const system = rest.filter((name) => name.startsWith('$'));
  return ['$default', ...user, ...system];
}

/**
 * Returns why the visual editor cannot represent this namespace, or `null`
 * when it can. Unknown keys are preserved by the editor but would be lost by a
 * whole-`allow` rewrite (templates), so the namespace becomes read-only.
 */
export function namespaceUnsupportedReason(
  rules: unknown,
  namespace: string,
): string | null {
  const block = namespaceBlock(rules, namespace);
  if (block === null) return null;

  for (const key of Object.keys(block)) {
    if (!KNOWN_NAMESPACE_KEYS.has(key)) {
      return `Unknown setting "${key}".`;
    }
  }

  const allow = block['allow'];
  if (allow !== undefined && allow !== null) {
    if (!isRecord(allow)) return 'allow must map actions to expressions.';
    for (const key of Object.keys(allow)) {
      if (!KNOWN_ALLOW_KEYS.has(key)) return `Unknown action "${key}".`;
    }
    for (const action of [...PERMISSION_ACTIONS, '$default'] as const) {
      const value = allow[action];
      if (
        value !== undefined &&
        value !== null &&
        typeof value !== 'string' &&
        typeof value !== 'boolean'
      ) {
        return `${action} must be a CEL expression string.`;
      }
    }
    for (const kind of ['link', 'unlink'] as const) {
      const nested = allow[kind];
      if (nested === undefined || nested === null) continue;
      if (!isRecord(nested))
        return `allow.${kind} must map labels to expressions.`;
      if (Object.values(nested).some((value) => typeof value !== 'string')) {
        return `allow.${kind} must map labels to expressions.`;
      }
    }
  }

  const fields = block['fields'];
  if (fields !== undefined && fields !== null) {
    if (!isRecord(fields)) return 'fields must map attributes to expressions.';
    if (Object.values(fields).some((value) => typeof value !== 'string')) {
      return 'fields must map attributes to expressions.';
    }
  }

  if (!readBinds(rules, namespace).valid) {
    return 'bind must be ["name", "expression", …] or {name: expression}.';
  }

  return null;
}

export type TemplateId =
  | 'public-read-owner-write'
  | 'signed-in'
  | 'admin-only'
  | 'read-only';

export interface PermissionTemplate {
  id: TemplateId;
  label: string;
  description: string;
}

export const PERMISSION_TEMPLATES: readonly PermissionTemplate[] = [
  {
    id: 'public-read-owner-write',
    label: 'Public read, owner write',
    description:
      'Anyone can read; only the owner can create, update or delete.',
  },
  {
    id: 'signed-in',
    label: 'Signed-in users only',
    description: 'Every action requires a signed-in user.',
  },
  {
    id: 'admin-only',
    label: 'Admin only',
    description: 'Deny view, create, update and delete for everyone.',
  },
  {
    id: 'read-only',
    label: 'Read-only for everyone',
    description: 'Anyone can read; every write is denied.',
  },
];

const OWNER_WRITE = "auth.id in data.ref('owner.id')";
const OWNER_CREATE = "auth.id in newData.ref('owner.id')";

export function templateAllow(
  template: TemplateId,
): Record<PermissionAction, string> {
  switch (template) {
    case 'public-read-owner-write':
      return {
        view: 'true',
        create: OWNER_CREATE,
        update: OWNER_WRITE,
        delete: OWNER_WRITE,
      };
    case 'signed-in':
      return {
        view: 'auth.id != null',
        create: 'auth.id != null',
        update: 'auth.id != null',
        delete: 'auth.id != null',
      };
    case 'admin-only':
      return {
        view: 'false',
        create: 'false',
        update: 'false',
        delete: 'false',
      };
    case 'read-only':
      return {
        view: 'true',
        create: 'false',
        update: 'false',
        delete: 'false',
      };
  }
}

/** Replaces the namespace's `allow` block; `bind` and `fields` stay. */
export function applyTemplate(
  rules: unknown,
  namespace: string,
  template: TemplateId,
): Rules {
  const document = cloneRules(rules);
  const block = ensureRecord(document, namespace);
  block['allow'] = templateAllow(template);
  return document;
}

export function actionRowKey(
  namespace: string,
  action: PermissionAction,
): string {
  return [namespace, 'allow', action].join('|');
}

export function relationshipRowKey(
  namespace: string,
  kind: RelationshipKind,
  label: string,
): string {
  return [namespace, 'allow', kind, label].join('|');
}

export function fieldRowKey(namespace: string, field: string): string {
  return [namespace, 'fields', field].join('|');
}

export function bindRowKey(namespace: string, name: string): string {
  return [namespace, 'bind', name].join('|');
}
