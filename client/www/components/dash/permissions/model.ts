import JSON5 from 'json5';
import { apply, type Edit } from '@/lib/editscript';
export type Rules = Record<string, unknown>;
export type RuleVersion = {
  version: number;
  edits: Edit[];
  created_at: string;
};
export const isRecord = (value: unknown): value is Rules =>
  value !== null && typeof value === 'object' && !Array.isArray(value);

export function recoverDraft(stored: string | null, saved: string) {
  try {
    const data = JSON.parse(stored ?? 'null');
    if (
      typeof data?.base === 'string' &&
      typeof data?.draft === 'string' &&
      data.base !== data.draft
    ) {
      return { base: data.base, draft: data.draft, recovered: true };
    }
  } catch {
    /* Ignore malformed browser storage. */
  }
  return { base: saved, draft: saved, recovered: false };
}

export function parseRules(text: string): Rules {
  const rules = JSON5.parse(text, (_key, value) =>
    typeof value === 'boolean' ? String(value) : value,
  );
  if (!isRecord(rules)) throw new Error('Rules must be a JSON object.');
  for (const [name, block] of Object.entries(rules)) {
    if (!isRecord(block))
      throw new Error(`${name} must contain a rule object.`);
    if (name === '$rateLimits') continue;
    for (const property of ['allow', 'fields']) {
      const values = block[property];
      if (values === undefined) continue;
      if (
        !isRecord(values) ||
        Object.values(values).some((v) => typeof v !== 'string')
      ) {
        throw new Error(
          `${name}.${property} must map names to expression strings.`,
        );
      }
    }
  }
  return rules;
}
export function ruleError(error: any): string {
  const validation = error?.body?.hint?.errors;
  if (Array.isArray(validation) && validation.length) {
    return validation
      .map(
        (v: any) =>
          `${Array.isArray(v.in) ? v.in.join(' → ') + ': ' : ''}${v.message || 'Invalid rule'}`,
      )
      .join('\n');
  }
  return (
    error?.body?.message ||
    error?.message ||
    'Could not save permissions. Your draft is still available; try again.'
  );
}
export function reconstructVersion(
  rules: Rules,
  currentVersion: number | null,
  versions: RuleVersion[],
  selected: number,
) {
  const sorted = [...versions].sort((a, b) => b.version - a.version);
  if (
    sorted[0]?.version !== currentVersion ||
    !sorted.some((v) => v.version === selected)
  )
    return null;
  for (let i = 1; i < sorted.length; i++) {
    if (sorted[i - 1].version - sorted[i].version !== 1) return null;
  }
  try {
    const reverse = (source: Rules, edits: Edit[]) => {
      let next = source;
      for (const edit of edits) {
        if (
          !Array.isArray(edit) ||
          !Array.isArray(edit[0]) ||
          !['+', '-', 'r'].includes(edit[1])
        )
          throw new Error('Invalid history edit');
        if (edit[0].length === 0) {
          if (edit[1] !== 'r' || !isRecord(edit[2]))
            throw new Error('Unsupported root edit');
          next = edit[2];
        } else next = apply(next, [edit]);
      }
      return next;
    };
    let result = rules;
    for (const version of sorted) {
      if (version.version === selected) {
        const previous = reverse(result, version.edits);
        return isRecord(previous) ? { rules: result, previous } : null;
      }
      result = reverse(result, version.edits);
      if (!isRecord(result)) return null;
    }
  } catch {
    return null;
  }
  return null;
}
export function outline(rules: Rules, names: string[], search: string) {
  const term = search.trim().toLowerCase();
  return [...new Set([...Object.keys(rules), ...names])]
    .sort()
    .filter((name) =>
      `${name} ${JSON.stringify(rules[name] ?? '')}`
        .toLowerCase()
        .includes(term),
    );
}
export function ruleSummary(block: unknown) {
  if (!isRecord(block)) return [];
  return Object.entries(block).flatMap(([group, entries]) => {
    if (!isRecord(entries)) return [[group, JSON.stringify(entries)]];
    return Object.entries(entries).map(([key, value]) => [
      `${group}.${key}`,
      typeof value === 'string' ? value : JSON.stringify(value),
    ]);
  });
}
