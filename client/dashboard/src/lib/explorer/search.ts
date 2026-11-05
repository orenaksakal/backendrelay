import type {
  FilterAttribute,
  FilterCondition,
  FilterOperator,
  InstaqlWhere,
} from '@/components/data/filter-serialization';
import { createFilterCondition } from '@/components/data/filter-serialization';

/**
 * C.6 search box: plain text searches every indexed string attribute with an
 * OR'd `$ilike`, while `attr:value` / `count>3` becomes a quick filter row.
 * The text search lives in the URL's `where` as the `or` clause, so a reload
 * (and the back button) restores the results and the box.
 */

export const TEXT_SEARCH_OPERATOR = '$ilike';

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function isUnknownArray(value: unknown): value is unknown[] {
  return Array.isArray(value);
}

/** Attributes the free-text search can scan: indexed strings only. */
export function indexedStringAttributes(
  attributes: ReadonlyArray<FilterAttribute>,
): FilterAttribute[] {
  return attributes.filter(
    (attribute) => attribute.type === 'string' && attribute.indexed === true,
  );
}

/** The InstaQL operator for a quick-filter separator, if it has one. */
export function quickFilterOperator(input: string): FilterOperator | undefined {
  switch (input) {
    case ':':
    case '=':
      return '$eq';
    case '!=':
      return '$ne';
    case '>':
      return '$gt';
    case '>=':
      return '$gte';
    case '<':
      return '$lt';
    case '<=':
      return '$lte';
    default:
      return undefined;
  }
}

const QUICK_FILTER_PATTERN = /(>=|<=|!=|=|:|>|<)/;

export type SearchInputParse =
  | { kind: 'search' }
  | { kind: 'filter'; condition: FilterCondition }
  | { kind: 'invalid'; message: string };

/**
 * Parses what the user typed. No operator means a text search; anything with
 * `:`, `=` or a comparison is a quick filter. Invalid input explains itself so
 * the toolbar can show inline help instead of dropping the query.
 */
export function parseSearchInput(
  input: string,
  attributes: ReadonlyArray<FilterAttribute>,
): SearchInputParse {
  const text = input.trim();
  if (text === '') return { kind: 'search' };

  const match = QUICK_FILTER_PATTERN.exec(text);
  if (match === null) return { kind: 'search' };
  if (match.index === 0) {
    return {
      kind: 'invalid',
      message: 'Start with an attribute name, for example done:false.',
    };
  }

  const operator = quickFilterOperator(match[1] ?? '');
  if (operator === undefined) {
    return { kind: 'search' };
  }

  const separator = match[1] ?? '';
  const attributeName = text.slice(0, match.index).trim();
  const value = text.slice(match.index + separator.length).trim();
  const attribute = attributes.find(
    (candidate) => candidate.name === attributeName,
  );
  if (attribute === undefined) {
    return {
      kind: 'invalid',
      message: `Unknown attribute "${attributeName}".`,
    };
  }
  if (value === '') {
    return {
      kind: 'invalid',
      message: `Enter a value after ${separator}.`,
    };
  }
  if (
    (operator === '$gt' ||
      operator === '$gte' ||
      operator === '$lt' ||
      operator === '$lte') &&
    attribute.indexed !== true
  ) {
    return {
      kind: 'invalid',
      message: `"${attributeName}" isn't indexed, so it can't be compared. Add an index or use Filter.`,
    };
  }

  return {
    kind: 'filter',
    condition: { ...createFilterCondition(attributeName, operator), value },
  };
}

/** The `or` clause that carries a free-text search, or `null` when it can't. */
export function textSearchClause(
  text: string,
  attributes: ReadonlyArray<FilterAttribute>,
): { or: InstaqlWhere[] } | null {
  const needle = text.trim();
  if (needle === '') return null;
  const targets = indexedStringAttributes(attributes);
  if (targets.length === 0) return null;

  return {
    or: targets.map((attribute) => ({
      [attribute.name]: { [TEXT_SEARCH_OPERATOR]: `%${needle}%` },
    })),
  };
}

function unwrapIlike(value: string): string | null {
  const match = /^%(.*)%$/s.exec(value);
  return match?.[1] ?? null;
}

/**
 * Splits the generated text-search `or` clause back out of a URL `where`.
 * Anything that doesn't look exactly like one of our clauses is left in place
 * for the filter parser instead of being guessed at.
 */
export function extractTextSearch(
  where: InstaqlWhere,
  attributes: ReadonlyArray<FilterAttribute>,
): { text: string; where: InstaqlWhere } {
  const raw = where['or'];
  const clauses = isUnknownArray(raw) ? raw : [];
  if (clauses.length === 0) {
    return { text: '', where };
  }

  const targets = new Set(
    indexedStringAttributes(attributes).map((attribute) => attribute.name),
  );
  let text: string | undefined;

  for (const clause of clauses) {
    if (!isRecord(clause)) return { text: '', where };
    const names = Object.keys(clause);
    const name = names[0];
    if (names.length !== 1 || name === undefined || !targets.has(name)) {
      return { text: '', where };
    }
    const operator = clause[name];
    if (!isRecord(operator)) return { text: '', where };
    const ilike = operator[TEXT_SEARCH_OPERATOR];
    if (typeof ilike !== 'string') return { text: '', where };
    const candidate = unwrapIlike(ilike);
    if (candidate === null) return { text: '', where };
    if (text !== undefined && text !== candidate) {
      return { text: '', where };
    }
    text = candidate;
  }

  if (text === undefined) return { text: '', where };
  const rest: InstaqlWhere = { ...where };
  delete rest['or'];
  return { text, where: rest };
}

/** Adds/removes a text search in an existing `where`. */
export function withTextSearch(
  where: InstaqlWhere,
  text: string,
  attributes: ReadonlyArray<FilterAttribute>,
): InstaqlWhere {
  const rest: InstaqlWhere = { ...where };
  delete rest['or'];
  const clause = textSearchClause(text, attributes);
  return clause === null ? rest : { ...rest, ...clause };
}
