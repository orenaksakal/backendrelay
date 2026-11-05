import type { EffectiveRole } from '@/lib/api/endpoints/schemas';
import { roleRank } from '@/lib/organization/model';

/**
 * C.10 permission rules model: the JSON document, the server's validation
 * errors, and the version history. The server records one editscript per rules
 * version, and each script transforms version N into version N-1 (see
 * `server-rs/crates/instant-server/migrations/102_rule_versions.up.sql`), so
 * reconstructing an older document means walking the edits backwards from the
 * current rules.
 */

export type Rules = Record<string, unknown>;

export type EditOp = '+' | '-' | 'r';

/** `[path, op, value?]`; a root replacement uses an empty path and `r`. */
export type Edit = [ReadonlyArray<string | number>, EditOp, ...unknown[]];

export interface RuleVersion {
  version: number;
  edits: readonly unknown[];
  created_at: string;
}

/** Collaborators may view and edit rules; the server allows role C. */
export function canEditPermissions(role: EffectiveRole): boolean {
  return roleRank(role) >= roleRank('collaborator');
}

/** Test-as runs the admin debug endpoints, so it needs role A (C.10). */
export function canTestPermissions(role: EffectiveRole): boolean {
  return roleRank(role) >= roleRank('admin');
}

export function isRules(value: unknown): value is Rules {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return isRules(value);
}

/** The canonical editor text for a rules document. */
export function rulesJson(rules: unknown): string {
  return JSON.stringify(isRules(rules) ? rules : {}, null, 2);
}

/** Namespace keys the JSON schema gives their own block. */
export function ruleNamespaces(rules: unknown): string[] {
  if (!isRules(rules)) return [];
  return Object.keys(rules).filter(
    (name) => name !== '$default' && name !== '$rateLimits' && name !== 'attrs',
  );
}

/**
 * Parses the JSON editor into the object `POST /dash/apps/:id/rules` expects.
 * CEL `true` and `false` are booleans in JSON and strings in the document, so
 * booleans are normalized the way the server expects; every block and
 * `allow`/`fields` map must map names to expression strings.
 */
export function parseRules(text: string): Rules {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text, (_key, value: unknown) =>
      typeof value === 'boolean' ? String(value) : value,
    );
  } catch {
    throw new Error('Rules must be valid JSON.');
  }
  if (!isRules(parsed)) {
    throw new Error('Rules must be a JSON object.');
  }
  for (const [name, block] of Object.entries(parsed)) {
    if (!isRules(block)) {
      throw new Error(`${name} must contain a rule object.`);
    }
    if (name === '$rateLimits') continue;
    for (const property of ['allow', 'fields']) {
      const values = block[property];
      if (values === undefined) continue;
      if (!isRules(values)) {
        throw new Error(
          `${name}.${property} must map names to expression strings.`,
        );
      }
      for (const [key, value] of Object.entries(values)) {
        if (typeof value === 'string') continue;
        // `allow.link`/`allow.unlink` map relationship labels to expressions.
        const relationship =
          property === 'allow' && (key === 'link' || key === 'unlink');
        if (
          relationship &&
          isRules(value) &&
          Object.values(value).every((entry) => typeof entry === 'string')
        ) {
          continue;
        }
        throw new Error(
          `${name}.${property} must map names to expression strings.`,
        );
      }
    }
  }
  return parsed;
}

function validationIssues(error: unknown): unknown[] | null {
  const candidates = [error];
  if (isRecord(error)) {
    candidates.push(error['body'], error['hint']);
  }
  for (const candidate of candidates) {
    if (!isRecord(candidate)) continue;
    const hint = candidate['hint'];
    const errors = isRecord(hint) ? hint['errors'] : candidate['errors'];
    if (Array.isArray(errors)) return errors as unknown[];
  }
  return null;
}

/**
 * One readable message for a failed save. The server returns
 * `hint.errors: [{path, message}]`, older payloads use `in`, and a transport
 * failure has neither, so the draft-recovery advice is the last resort.
 */
export function ruleError(error: unknown): string {
  const issues = validationIssues(error);
  if (issues !== null && issues.length > 0) {
    return issues
      .map((issue) => {
        const record = isRecord(issue) ? issue : {};
        const path = Array.isArray(record['path'])
          ? record['path']
          : Array.isArray(record['in'])
            ? record['in']
            : [];
        const message =
          typeof record['message'] === 'string' && record['message'] !== ''
            ? record['message']
            : 'Invalid rule';
        const prefix =
          path.length > 0 ? `${path.map(String).join(' → ')}: ` : '';
        return `${prefix}${message}`;
      })
      .join('\n');
  }
  if (error instanceof Error && error.message.trim() !== '') {
    return error.message;
  }
  return 'Could not save permissions. Your draft is still available; try again.';
}

/** Newest first, which is the order the history list and reconstruction use. */
export function sortedVersions(
  versions: readonly RuleVersion[],
): RuleVersion[] {
  return [...versions].sort((left, right) => right.version - left.version);
}

/**
 * The edit chain is only usable when it starts at the live rules version and
 * has no gaps; otherwise a restored document would be silently wrong.
 */
export function versionsInSync(
  currentVersion: number | null | undefined,
  versions: readonly RuleVersion[],
): boolean {
  const latest = sortedVersions(versions)[0];
  return currentVersion != null && latest?.version === currentVersion;
}

function isEditOp(value: unknown): value is EditOp {
  return value === '+' || value === '-' || value === 'r';
}

function isPath(value: unknown): value is ReadonlyArray<string | number> {
  return (
    Array.isArray(value) &&
    value.every((part) => typeof part === 'string' || typeof part === 'number')
  );
}

function isEmptyValue(value: unknown): boolean {
  return (
    value !== null &&
    typeof value === 'object' &&
    Object.keys(value).length === 0
  );
}

function insertIn(
  root: unknown,
  path: ReadonlyArray<string | number>,
  value: unknown,
): void {
  if (path.length === 0) return;
  let current: unknown = root;
  for (let index = 0; index < path.length - 1; index++) {
    const key = path[index];
    if (key === undefined) return;
    const holder = current as Record<string, unknown>;
    const child = holder[String(key)];
    if (child === null || typeof child !== 'object') {
      holder[String(key)] = typeof path[index + 1] === 'number' ? [] : {};
    }
    current = holder[String(key)];
  }
  const last = path[path.length - 1];
  if (last === undefined) return;
  if (Array.isArray(current) && typeof last === 'number') {
    current.splice(last, 0, value);
    return;
  }
  (current as Record<string, unknown>)[String(last)] = value;
}

function assocIn(
  root: unknown,
  path: ReadonlyArray<string | number>,
  value: unknown,
): void {
  if (path.length === 0) return;
  let current: unknown = root;
  for (let index = 0; index < path.length - 1; index++) {
    const key = path[index];
    if (key === undefined) return;
    const holder = current as Record<string, unknown>;
    const child = holder[String(key)];
    if (child === null || typeof child !== 'object') {
      holder[String(key)] = typeof path[index + 1] === 'number' ? [] : {};
    }
    current = holder[String(key)];
  }
  const last = path[path.length - 1];
  if (last === undefined) return;
  if (Array.isArray(current) && typeof last === 'number') {
    current[last] = value;
    return;
  }
  (current as Record<string, unknown>)[String(last)] = value;
}

function dissocIn(root: unknown, path: ReadonlyArray<string | number>): void {
  if (path.length === 0 || root === null || typeof root !== 'object') return;
  const [key, ...rest] = path;
  if (key === undefined) return;
  const holder = root as Record<string, unknown>;
  const childKey = String(key);
  if (!(childKey in holder)) return;
  if (rest.length === 0) {
    if (Array.isArray(root) && typeof key === 'number') {
      root.splice(key, 1);
    } else {
      Reflect.deleteProperty(holder, childKey);
    }
    return;
  }
  dissocIn(holder[childKey], rest);
  if (isEmptyValue(holder[childKey])) {
    Reflect.deleteProperty(holder, childKey);
  }
}

/** Applies forward edits to a copy; version history uses the reverse chain. */
export function applyEdits(base: Rules, edits: readonly Edit[]): Rules {
  const draft = JSON.parse(JSON.stringify(base)) as Rules;
  for (const [path, op, value] of edits) {
    if (op === '+') insertIn(draft, path, value);
    else if (op === 'r') assocIn(draft, path, value);
    else dissocIn(draft, path);
  }
  return draft;
}

function reverseEdits(source: Rules, edits: readonly unknown[]): Rules {
  let next: Rules = source;
  for (const edit of edits) {
    if (!Array.isArray(edit) || edit.length < 2) {
      throw new Error('Invalid history edit');
    }
    const items = edit as unknown[];
    const path = items[0];
    const op = items[1];
    if (!isPath(path) || !isEditOp(op)) {
      throw new Error('Invalid history edit');
    }
    if (path.length === 0) {
      const value = items[2];
      if (op !== 'r' || !isRules(value)) {
        throw new Error('Unsupported root edit');
      }
      next = value;
      continue;
    }
    const applied: Edit = [path, op, items[2]];
    next = applyEdits(next, [applied]);
  }
  return next;
}

/**
 * Rebuilds the rules document at `selected` from the live rules and the
 * reverse edit chain. Returns `null` when history is out of sync, has a gap or
 * contains an edit this client cannot apply.
 */
export function reconstructVersion(
  rules: Rules,
  currentVersion: number | null | undefined,
  versions: readonly RuleVersion[],
  selected: number,
): { rules: Rules; previous: Rules } | null {
  const sorted = sortedVersions(versions);
  if (
    currentVersion == null ||
    sorted[0]?.version !== currentVersion ||
    !sorted.some((version) => version.version === selected)
  ) {
    return null;
  }
  for (let index = 1; index < sorted.length; index++) {
    const current = sorted[index];
    const previous = sorted[index - 1];
    if (
      current === undefined ||
      previous === undefined ||
      previous.version - current.version !== 1
    ) {
      return null;
    }
  }

  try {
    let result = rules;
    for (const version of sorted) {
      if (version.version === selected) {
        const previous = reverseEdits(result, version.edits);
        return { rules: result, previous };
      }
      result = reverseEdits(result, version.edits);
    }
  } catch {
    return null;
  }
  return null;
}
