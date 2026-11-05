/**
 * Filter rows ↔ InstaQL `where` objects (C.6).
 *
 * The Explorer keeps filters in the URL as JSON. This module is the single
 * translation point: rows carry a string value for editing, `where` carries
 * typed values for the query.
 */

export type FilterValueType =
  | 'string'
  | 'number'
  | 'boolean'
  | 'date'
  | 'json'
  | 'any';

export interface FilterAttribute {
  name: string;
  type: FilterValueType;
  /** Comparison operators are only offered for indexed attributes. */
  indexed?: boolean;
}

export const FILTER_OPERATORS = [
  { value: '$eq', label: 'Equals' },
  { value: '$ne', label: 'Not equals' },
  { value: '$gt', label: 'Greater than' },
  { value: '$gte', label: 'Greater than or equal' },
  { value: '$lt', label: 'Less than' },
  { value: '$lte', label: 'Less than or equal' },
  { value: '$like', label: 'Matches (case-sensitive)' },
  { value: '$ilike', label: 'Matches (case-insensitive)' },
  { value: '$in', label: 'Is one of' },
  { value: '$isNull', label: 'Is null' },
] as const;

export type FilterOperator = (typeof FILTER_OPERATORS)[number]['value'];

const OPERATOR_VALUES = new Set<string>(
  FILTER_OPERATORS.map((operator) => operator.value),
);

/** Comparison operators need an index server-side. */
const COMPARISON_OPERATORS = new Set<FilterOperator>([
  '$gt',
  '$gte',
  '$lt',
  '$lte',
]);

export interface FilterCondition {
  id: string;
  enabled: boolean;
  attribute: string;
  operator: FilterOperator;
  /** Raw text from the value input; coerced on serialization. */
  value: string;
}

export type InstaqlWhere = Record<string, unknown>;

let nextId = 0;

function createId(): string {
  nextId += 1;
  return `filter-${String(nextId)}`;
}

export function createFilterCondition(
  attribute = '',
  operator: FilterOperator = '$eq',
): FilterCondition {
  return { id: createId(), enabled: true, attribute, operator, value: '' };
}

export function isOperatorSupported(
  operator: FilterOperator,
  attribute: FilterAttribute | undefined,
): boolean {
  if (!COMPARISON_OPERATORS.has(operator)) {
    return true;
  }
  return attribute?.indexed === true;
}

function coerceValue(raw: string, type: FilterValueType): unknown {
  switch (type) {
    case 'number': {
      const parsed = Number(raw);
      return Number.isFinite(parsed) ? parsed : raw;
    }
    case 'boolean':
      if (raw === 'true') {
        return true;
      }
      if (raw === 'false') {
        return false;
      }
      return raw;
    case 'json': {
      try {
        return JSON.parse(raw) as unknown;
      } catch {
        return raw;
      }
    }
    default:
      return raw;
  }
}

function coerceList(raw: string, type: FilterValueType): unknown[] {
  return raw
    .split(',')
    .map((part) => part.trim())
    .filter((part) => part !== '')
    .map((part) => coerceValue(part, type));
}

function isOperatorObject(value: unknown): value is Record<string, unknown> {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    return false;
  }
  const keys = Object.keys(value);
  return keys.length > 0 && keys.every((key) => key.startsWith('$'));
}

function assign(
  where: InstaqlWhere,
  attribute: string,
  operator: FilterOperator,
  value: unknown,
) {
  const existing = where[attribute];

  if (operator === '$eq' && !isOperatorObject(existing)) {
    where[attribute] = value;
    return;
  }

  const target: Record<string, unknown> = isOperatorObject(existing)
    ? { ...existing }
    : existing === undefined
      ? {}
      : { $eq: existing };
  target[operator] = value;
  where[attribute] = target;
}

/**
 * Rows → `where`. Disabled rows, rows without an attribute and rows without a
 * value are dropped. Several rows on one attribute merge into one operator
 * object, with a plain value becoming an explicit `$eq`.
 */
export function serializeFilters(
  conditions: ReadonlyArray<FilterCondition>,
  attributes: ReadonlyArray<FilterAttribute>,
): InstaqlWhere {
  const types = new Map(
    attributes.map((attribute) => [attribute.name, attribute]),
  );
  const where: InstaqlWhere = {};

  for (const condition of conditions) {
    if (!condition.enabled || condition.attribute === '') {
      continue;
    }
    if (!types.has(condition.attribute)) {
      continue;
    }
    const type = types.get(condition.attribute)?.type ?? 'any';

    if (condition.operator === '$isNull') {
      assign(
        where,
        condition.attribute,
        '$isNull',
        condition.value === '' ? true : condition.value === 'true',
      );
      continue;
    }

    if (condition.value === '') {
      continue;
    }

    if (condition.operator === '$in') {
      const values = coerceList(condition.value, type);
      if (values.length > 0) {
        assign(where, condition.attribute, '$in', values);
      }
      continue;
    }

    assign(
      where,
      condition.attribute,
      condition.operator,
      coerceValue(condition.value, type),
    );
  }

  return where;
}

function formatValue(value: unknown, type: FilterValueType): string {
  if (value === null) {
    return '';
  }
  if (Array.isArray(value)) {
    return value.map((part) => formatValue(part, type)).join(', ');
  }
  if (typeof value === 'object') {
    return JSON.stringify(value);
  }
  if (typeof value === 'string') {
    return value;
  }
  if (
    typeof value === 'number' ||
    typeof value === 'boolean' ||
    typeof value === 'bigint'
  ) {
    return String(value);
  }
  return '';
}

function toOperator(key: string): FilterOperator {
  return OPERATOR_VALUES.has(key) ? (key as FilterOperator) : '$eq';
}

/**
 * `where` → rows, for a URL round trip or for rendering filters someone else
 * wrote. Unrecognised operators fall back to `$eq`.
 */
export function parseWhere(
  where: InstaqlWhere,
  attributes: ReadonlyArray<FilterAttribute>,
): FilterCondition[] {
  const types = new Map(
    attributes.map((attribute) => [attribute.name, attribute]),
  );
  const conditions: FilterCondition[] = [];

  for (const [attribute, value] of Object.entries(where)) {
    const type = types.get(attribute)?.type ?? 'any';

    if (isOperatorObject(value)) {
      for (const [operator, operand] of Object.entries(value)) {
        const parsedOperator = toOperator(operator);
        conditions.push({
          id: createId(),
          enabled: true,
          attribute,
          operator: parsedOperator,
          value:
            parsedOperator === '$isNull'
              ? operand === false
                ? 'false'
                : 'true'
              : formatValue(operand, type),
        });
      }
      continue;
    }

    conditions.push({
      id: createId(),
      enabled: true,
      attribute,
      operator: '$eq',
      value: formatValue(value, type),
    });
  }

  return conditions;
}

/** URL form of the filters; an empty string means "no filters". */
export function serializeWhereParam(
  conditions: ReadonlyArray<FilterCondition>,
  attributes: ReadonlyArray<FilterAttribute>,
): string {
  const where = serializeFilters(conditions, attributes);
  return Object.keys(where).length === 0 ? '' : JSON.stringify(where);
}

/** Parse the URL form; malformed input yields no filters rather than failing. */
export function parseWhereParam(
  param: string | undefined,
  attributes: ReadonlyArray<FilterAttribute>,
): FilterCondition[] {
  if (!param) {
    return [];
  }
  try {
    const parsed: unknown = JSON.parse(param);
    if (
      parsed === null ||
      typeof parsed !== 'object' ||
      Array.isArray(parsed)
    ) {
      return [];
    }
    return parseWhere(parsed as InstaqlWhere, attributes);
  } catch {
    return [];
  }
}
