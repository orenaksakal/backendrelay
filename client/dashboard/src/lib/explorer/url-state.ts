import { z } from 'zod';
import {
  parseWhere,
  serializeFilters,
  type FilterAttribute,
  type FilterCondition,
  type InstaqlWhere,
} from '@/components/data/filter-serialization';
import { appHref } from '@/lib/navigation';
import { extractTextSearch, textSearchClause } from './search';

/**
 * Explorer URL state (C.6, B.6): `ns`, `where` (JSON), `sort`, `dir` and
 * `size`. Every view change goes through here so the browser's back/forward
 * buttons step through filter history and a copied URL restores the screen.
 */

export const DEFAULT_EXPLORER_PAGE_SIZE = 100;
export const EXPLORER_PAGE_STEP = 100;
export const MAX_EXPLORER_PAGE_SIZE = 10_000;

export interface ExplorerSort {
  attribute: string;
  direction: 'asc' | 'desc';
}

export interface ExplorerUrlState {
  namespace: string | null;
  /** Free-text search, carried in the URL as the `or` clause of `where`. */
  text: string;
  conditions: FilterCondition[];
  sort: ExplorerSort | null;
  size: number;
  /** `?entity=`: the entity sheet target (C.6). */
  entity?: string | null;
  /** `?new=1`: the entity sheet is open for a new entity. */
  creating?: boolean;
}

/**
 * Search params are validated permissively: a hand-edited URL falls back to
 * the default instead of failing the route.
 */
export const explorerSearchSchema = z.object({
  ns: z.string().trim().min(1).optional().catch(undefined),
  where: z.string().optional().catch(undefined),
  sort: z.string().trim().min(1).optional().catch(undefined),
  dir: z.enum(['asc', 'desc']).catch('asc'),
  size: z.coerce
    .number()
    .int()
    .min(1)
    .max(MAX_EXPLORER_PAGE_SIZE)
    .catch(DEFAULT_EXPLORER_PAGE_SIZE),
  entity: z.string().trim().min(1).optional().catch(undefined),
  new: z
    .union([z.literal('1'), z.literal(1)])
    .optional()
    .catch(undefined),
});

export type ExplorerSearch = z.infer<typeof explorerSearchSchema>;

function parseWhereParam(param: string | undefined): InstaqlWhere {
  if (param === undefined || param === '') return {};
  try {
    const parsed: unknown = JSON.parse(param);
    if (
      parsed === null ||
      typeof parsed !== 'object' ||
      Array.isArray(parsed)
    ) {
      return {};
    }
    return parsed as InstaqlWhere;
  } catch {
    return {};
  }
}

/** `where` for a namespace's filters plus the current text search. */
export function serializeExplorerWhere(
  { text, conditions }: Pick<ExplorerUrlState, 'text' | 'conditions'>,
  attributes: ReadonlyArray<FilterAttribute>,
): InstaqlWhere | null {
  const where = serializeFilters(conditions, attributes);
  const search = textSearchClause(text, attributes);
  const merged = search === null ? where : { ...where, ...search };
  return Object.keys(merged).length === 0 ? null : merged;
}

export function readExplorerUrlState(
  search: ExplorerSearch,
  attributes: ReadonlyArray<FilterAttribute>,
): ExplorerUrlState {
  const { text, where } = extractTextSearch(
    parseWhereParam(search.where),
    attributes,
  );
  // An `or`/`and` clause we can't reconstruct as a text search is not a
  // filter row; dropping it beats showing a pseudo-attribute named "or".
  const filters: InstaqlWhere = { ...where };
  delete filters['or'];
  delete filters['and'];
  return {
    namespace: search.ns ?? null,
    text,
    conditions: parseWhere(filters, attributes),
    sort: search.sort
      ? { attribute: search.sort, direction: search.dir }
      : null,
    size: search.size,
    entity: search.entity ?? null,
    creating: search.new === '1' || search.new === 1,
  };
}

/** The URL form of a view state; empty values are dropped from the URL. */
export function explorerSearchFor(
  state: ExplorerUrlState,
  attributes: ReadonlyArray<FilterAttribute>,
): ExplorerSearch {
  const where = serializeExplorerWhere(state, attributes);
  return {
    ns: state.namespace ?? undefined,
    where: where === null ? undefined : JSON.stringify(where),
    sort: state.sort?.attribute,
    dir: state.sort?.direction ?? 'asc',
    size: state.size,
    entity: state.entity ?? undefined,
    new: state.creating === true ? '1' : undefined,
  };
}

/**
 * A sort survives only while its attribute exists in the selected namespace;
 * switching namespaces therefore drops a stale order instead of querying it.
 */
export function resolveSort(
  sort: ExplorerSort | null,
  attributes: ReadonlyArray<FilterAttribute>,
): ExplorerSort | null {
  if (sort === null) return null;
  return attributes.some((attribute) => attribute.name === sort.attribute)
    ? sort
    : null;
}

/** Link into another namespace, optionally already filtered to one entity. */
export function explorerHref(
  appId: string,
  namespace: string,
  where?: InstaqlWhere,
): string {
  const params = new URLSearchParams({ ns: namespace });
  if (where !== undefined && Object.keys(where).length > 0) {
    params.set('where', JSON.stringify(where));
  }
  return `${appHref(appId, 'explorer')}?${params.toString()}`;
}
