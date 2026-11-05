import { coerceQuery, weakHash } from '@instantdb/core';
import { InstantReactAbstractDatabase } from '@instantdb/react';
import React, {
  useCallback,
  useEffect,
  useRef,
  useState,
  useSyncExternalStore,
} from 'react';
import { DBAttr, SchemaNamespace } from '@lib/types';
import { dbAttrsToExplorerSchema } from '@lib/schema';

export type SearchFilterOp =
  | '='
  | '$ilike'
  | '$like'
  | '$gt'
  | '$lt'
  | '$isNull';
export type SearchFilter = [string, SearchFilterOp, any];

// When navWhere is set (relationship link navigation), ignore stale local
// search filters from the previous table.
export function resolveFilters(
  navWhere: [string, any] | undefined,
  navFilters: SearchFilter[] | undefined,
  localSearchFilters: SearchFilter[],
): SearchFilter[] {
  if (navFilters) return navFilters;
  if (navWhere) return [];
  return localSearchFilters;
}

export function makeWhere(
  navWhere: null | undefined | [string, any],
  searchFilters: null | undefined | SearchFilter[],
) {
  const where: { [key: string]: any } = {};
  if (navWhere) {
    where[navWhere[0]] = navWhere[1];
  }
  if (searchFilters?.length) {
    where.or = searchFilters.map(([attr, op, val]) => {
      switch (op) {
        case '=':
          return { [attr]: val };
        case '$isNull':
          return { [attr]: { [op]: true } };
        default:
          return { [attr]: { [op]: val } };
      }
    });
  }
  return where;
}

const initialQueryState = {
  isLoading: true,
  data: undefined,
  pageInfo: undefined,
  error: undefined,
} as const;

function stateForQueryResult(result: any) {
  return {
    ...initialQueryState,
    isLoading: !Boolean(result),
    ...(result ?? {}),
  };
}

export function makeExplorerQuerySubscriptionKey(
  queryHash: string,
  refreshKey: number,
) {
  return `${queryHash}:${refreshKey}`;
}

// Instant does not currently expose query invalidation. Including refreshKey in
// the subscription identity cleanly removes and re-adds the same server query.
function useRefreshableExplorerQuery(
  db: InstantReactAbstractDatabase<any, any>,
  queryInput: any,
  refreshKey: number,
) {
  const query = queryInput ? coerceQuery(queryInput) : null;
  const queryHash = weakHash(query);
  const subscriptionKey = makeExplorerQuerySubscriptionKey(
    queryHash,
    refreshKey,
  );
  const resultRef = useRef<any>(
    stateForQueryResult(
      query ? db.core._reactor.getPreviousResult(query) : undefined,
    ),
  );

  const subscribe = useCallback(
    (onStoreChange: () => void) => {
      resultRef.current = stateForQueryResult(
        query ? db.core._reactor.getPreviousResult(query) : undefined,
      );
      onStoreChange();

      if (!query) return () => {};

      return (db.core as any).subscribeQuery(query, (result: any) => {
        resultRef.current = stateForQueryResult(result);
        onStoreChange();
      });
    },
    [db, subscriptionKey],
  );

  return useSyncExternalStore(
    subscribe,
    () => resultRef.current,
    () => initialQueryState,
  );
}

export function getExplorerErrorMessage(error: unknown) {
  if (typeof error === 'string') return error;
  if (error && typeof error === 'object') {
    const body = 'body' in error ? error.body : undefined;
    if (
      body &&
      typeof body === 'object' &&
      'message' in body &&
      typeof body.message === 'string'
    ) {
      return body.message;
    }
    if ('message' in error && typeof error.message === 'string') {
      return error.message;
    }
  }
  return 'Unknown error';
}

export function ExplorerQueryError({
  error,
  namespace,
}: {
  error: unknown;
  namespace: string;
}) {
  return (
    <div
      role="alert"
      className="m-4 rounded-sm border border-red-300 bg-red-50 p-3 text-sm text-red-800 dark:border-red-900 dark:bg-red-950/40 dark:text-red-200"
    >
      <div className="font-semibold">Failed to load {namespace}</div>
      <div>{getExplorerErrorMessage(error)}</div>
    </div>
  );
}

// HOOKS
export function useNamespacesQuery(
  db: InstantReactAbstractDatabase<any, any>,
  selectedNs?: SchemaNamespace,
  navWhere?: [string, any],
  searchFilters?: SearchFilter[],
  limit?: number,
  offset?: number,
  sortAttr?: string,
  sortAsc?: boolean,
  refreshKey = 0,
) {
  const direction: 'asc' | 'desc' = sortAsc ? 'asc' : 'desc';

  const where = makeWhere(navWhere, searchFilters);

  const iql = selectedNs
    ? {
        [selectedNs.name]: {
          ...Object.fromEntries(
            selectedNs.attrs
              .filter((a) => a.type === 'ref')
              .map((a) => [a.name, { $: { fields: ['id'] } }]),
          ),
          $: {
            ...(where ? { where: where } : {}),
            ...(limit ? { limit } : {}),
            ...(offset ? { offset } : {}),
            ...(sortAttr ? { order: { [sortAttr]: direction } } : {}),
          },
        },
      }
    : {};

  const itemsRes = useRefreshableExplorerQuery(db, iql, refreshKey);

  const allRes = useRefreshableExplorerQuery(
    db,
    selectedNs
      ? {
          [selectedNs.name]: {
            $: {
              aggregate: 'count',
              ...(where ? { where: where } : {}),
            },
          },
        }
      : {},
    refreshKey,
  );

  const allCount = allRes.aggregate?.[selectedNs?.name ?? '']?.count ?? null;

  return {
    itemsRes,
    allCount,
    queryError: itemsRes.error ?? allRes.error,
  };
}
export function useSchemaQuery(db: InstantReactAbstractDatabase<any, any>) {
  const [state, setState] = useState<
    | {
        namespaces: SchemaNamespace[];
        attrs: Record<string, DBAttr>;
      }
    | { namespaces: null; attrs: null }
  >({ namespaces: null, attrs: null });

  // (XXX)
  // This is a hack so we can listen to all attr changes
  //
  // Context:
  // The backend only sends attr changes to relevant queries.
  // The ___explorer__ is a dummy query, which refreshes when _anything_
  // happens.
  //
  // In the future, we may want a special `attr-changed` event.
  db.useQuery({ ____explorer___: {} });

  useEffect(() => {
    function onAttrs(_oAttrs: Record<string, DBAttr>) {
      setState({
        attrs: _oAttrs,
        namespaces: dbAttrsToExplorerSchema(_oAttrs),
      });
    }
    return db._core._reactor.subscribeAttrs(onAttrs);
  }, [db]);

  return state;
}
