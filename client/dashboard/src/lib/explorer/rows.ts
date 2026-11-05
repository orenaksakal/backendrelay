import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import type { InstaqlWhere } from '@/components/data/filter-serialization';
import type { AppInstantClient } from '@/lib/instant';
import { isHighVolume, holdsLiveUpdates, recordUpdate } from './live';
import type { ExplorerSort } from './url-state';

/**
 * Live queries for the Explorer (C.6): one subscription for the visible page
 * of a namespace and one aggregate query for the sidebar counts. Results are
 * keyed by the query they belong to, so switching namespace never flashes the
 * previous rows back onto the grid.
 */

export interface ExplorerRow {
  id?: unknown;
  [attribute: string]: unknown;
}

export interface ExplorerQueryResult {
  rows: ExplorerRow[];
  hasNextPage: boolean;
}

interface RowsSnapshot extends ExplorerQueryResult {
  key: string;
  loaded: boolean;
  error: string | null;
}

export interface ExplorerRows {
  rows: ExplorerRow[];
  hasNextPage: boolean;
  isLoading: boolean;
  error: string | null;
  /** A newer result is waiting behind Refresh (paused or sorted/filtered). */
  pending: boolean;
  paused: boolean;
  /** Paused automatically because updates exceeded the live threshold. */
  volumePaused: boolean;
  togglePause: () => void;
  resume: () => void;
  refresh: () => void;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function isUnknownArray(value: unknown): value is unknown[] {
  return Array.isArray(value);
}

/** The row list and next-page flag from a `subscribeQuery` result. */
export function parseExplorerResult(
  result: unknown,
  namespace: string,
): ExplorerQueryResult {
  const payload: Record<string, unknown> = isRecord(result) ? result : {};
  const data = isRecord(payload['data'])
    ? payload['data'][namespace]
    : undefined;
  const rows = isUnknownArray(data)
    ? data.filter((row): row is ExplorerRow => isRecord(row))
    : [];

  const pageInfo = isRecord(payload['pageInfo'])
    ? payload['pageInfo'][namespace]
    : undefined;
  const hasNextPage = isRecord(pageInfo) && pageInfo['hasNextPage'] === true;

  return { rows, hasNextPage };
}

/** A query failure message, or `null` for a successful result. */
export function parseExplorerError(result: unknown): string | null {
  if (!isRecord(result)) return null;
  const error = result['error'];
  if (!isRecord(error)) return null;
  const message = error['message'];
  return typeof message === 'string' && message !== ''
    ? message
    : 'The query failed.';
}

/** `{ ns: { $: { where, order, limit }, link: { $: { fields } } } }`. */
export function buildExplorerQuery(
  namespace: string,
  {
    where,
    sort,
    limit,
    linkLabels,
  }: {
    where: InstaqlWhere;
    sort: ExplorerSort | null;
    limit: number;
    /** Link labels to fetch as id-only fields (C.6 link columns). */
    linkLabels?: ReadonlyArray<string>;
  },
): Record<string, unknown> {
  const options: Record<string, unknown> = { limit };
  if (Object.keys(where).length > 0) options['where'] = where;
  if (sort !== null) options['order'] = { [sort.attribute]: sort.direction };
  const form: Record<string, unknown> = {};
  for (const label of linkLabels ?? []) {
    form[label] = { $: { fields: ['id'] } };
  }
  form['$'] = options;
  return { [namespace]: form };
}

/** One aggregate query covers every namespace in the sidebar. */
export function buildCountsQuery(
  namespaces: ReadonlyArray<string>,
): Record<string, unknown> {
  const query: Record<string, unknown> = {};
  for (const namespace of namespaces) {
    query[namespace] = { $: { aggregate: 'count' } };
  }
  return query;
}

export function readAggregateCounts(
  result: unknown,
  namespaces: ReadonlyArray<string>,
): Record<string, number> {
  if (!isRecord(result)) return {};
  const aggregate = result['aggregate'];
  if (!isRecord(aggregate)) return {};
  const counts: Record<string, number> = {};
  for (const namespace of namespaces) {
    const entry = aggregate[namespace];
    if (!isRecord(entry)) continue;
    const count = entry['count'];
    if (typeof count === 'number') counts[namespace] = count;
  }
  return counts;
}

/** Sidebar totals; an unsupported aggregate simply leaves the count blank. */
export function useNamespaceCounts(
  client: AppInstantClient | null,
  namespaces: ReadonlyArray<string>,
): Record<string, number> {
  const [counts, setCounts] = useState<Record<string, number>>({});

  useEffect(() => {
    if (client === null || namespaces.length === 0) return;
    return client.core.subscribeQuery(
      buildCountsQuery(namespaces),
      (result) => {
        setCounts(readAggregateCounts(result, namespaces));
      },
    );
  }, [client, namespaces]);

  return counts;
}

export interface ExplorerEntity {
  entity: ExplorerRow | null;
  isLoading: boolean;
  error: string | null;
}

/** Stable empty list so hooks do not resubscribe on every render. */
const NO_LINKS: ReadonlyArray<string> = [];

interface EntitySnapshot {
  key: string;
  entity: ExplorerRow | null;
  error: string | null;
}

/**
 * One entity by id for the entity sheet (C.6). The sheet can be opened from a
 * link, so the row is not necessarily on the page the grid loaded; a dedicated
 * one-row subscription keeps the sheet live and correct either way.
 */
export function useExplorerEntity(
  client: AppInstantClient | null,
  namespace: string | null,
  entityId: string | null,
  linkLabels: ReadonlyArray<string> = NO_LINKS,
): ExplorerEntity {
  const key = JSON.stringify({ namespace, entityId, linkLabels });
  const [snapshot, setSnapshot] = useState<EntitySnapshot | null>(null);

  useEffect(() => {
    if (client === null || namespace === null || entityId === null) {
      return;
    }
    return client.core.subscribeQuery(
      buildExplorerQuery(namespace, {
        where: { id: entityId },
        sort: null,
        limit: 1,
        linkLabels,
      }),
      (result) => {
        const failure = parseExplorerError(result);
        if (failure !== null) {
          setSnapshot({ key, entity: null, error: failure });
          return;
        }
        // An Instant subscription can report loading before it has a page; a
        // missing row list must not look like "this entity has no values".
        const data = isRecord(result) ? result['data'] : undefined;
        const page = isRecord(data) ? data[namespace] : undefined;
        if (!isUnknownArray(page)) return;
        const parsed = parseExplorerResult(result, namespace);
        setSnapshot({ key, entity: parsed.rows[0] ?? null, error: null });
      },
    );
  }, [client, namespace, entityId, key, linkLabels]);

  const current = snapshot !== null && snapshot.key === key ? snapshot : null;

  return {
    entity: current?.entity ?? null,
    error: current?.error ?? null,
    isLoading:
      current === null &&
      client !== null &&
      namespace !== null &&
      entityId !== null,
  };
}

export interface UseExplorerRowsOptions {
  client: AppInstantClient | null;
  namespace: string | null;
  where: InstaqlWhere;
  sort: ExplorerSort | null;
  limit: number;
  /** Link labels the rows should include as id-only fields (C.6). */
  linkLabels?: ReadonlyArray<string>;
}

/**
 * Paged, live rows for one namespace. A sorted or filtered list holds new
 * results behind Refresh instead of reordering rows, and a burst of updates
 * pauses live data until the user resumes (B.5.5).
 */
export function useExplorerRows({
  client,
  namespace,
  where,
  sort,
  limit,
  linkLabels = NO_LINKS,
}: UseExplorerRowsOptions): ExplorerRows {
  const queryKey = useMemo(
    () => JSON.stringify({ namespace, where, sort, limit, linkLabels }),
    [namespace, where, sort, limit, linkLabels],
  );
  const [snapshot, setSnapshot] = useState<RowsSnapshot | null>(null);
  const [held, setHeld] = useState<{
    key: string;
    result: ExplorerQueryResult;
  } | null>(null);
  const [paused, setPaused] = useState(false);
  const [volumePaused, setVolumePaused] = useState(false);
  const [nonce, setNonce] = useState(0);
  const latest = useRef<ExplorerQueryResult | null>(null);
  const hasData = useRef(false);
  const lastKey = useRef('');
  const updates = useRef<number[]>([]);
  const live = !paused && !volumePaused;
  const liveRef = useRef(true);

  const apply = useCallback(
    (result: ExplorerQueryResult) => {
      setSnapshot({
        key: queryKey,
        rows: result.rows,
        hasNextPage: result.hasNextPage,
        loaded: true,
        error: null,
      });
      setHeld(null);
      hasData.current = true;
    },
    [queryKey],
  );

  useEffect(() => {
    liveRef.current = live;
  }, [live]);

  useEffect(() => {
    if (client === null || namespace === null) {
      latest.current = null;
      hasData.current = false;
      return;
    }
    // A new namespace, filter, sort or page size starts from an empty list;
    // a retry (`nonce`) keeps the rows that are already on screen.
    if (lastKey.current !== queryKey) {
      lastKey.current = queryKey;
      latest.current = null;
      hasData.current = false;
      updates.current = [];
    }

    return client.core.subscribeQuery(
      buildExplorerQuery(namespace, { where, sort, limit, linkLabels }),
      (result) => {
        const failure = parseExplorerError(result);
        if (failure !== null) {
          setSnapshot({
            key: queryKey,
            rows: [],
            hasNextPage: false,
            loaded: true,
            error: failure,
          });
          setHeld(null);
          return;
        }

        const parsed = parseExplorerResult(result, namespace);
        latest.current = parsed;
        updates.current = recordUpdate(updates.current, Date.now());

        // Pausing governs following changes, never the first page of a new
        // query: a filter change still shows its matches while paused.
        if (
          hasData.current &&
          (!liveRef.current ||
            holdsLiveUpdates(sort !== null, Object.keys(where).length > 0))
        ) {
          setHeld({ key: queryKey, result: parsed });
          return;
        }

        apply(parsed);
        if (isHighVolume(updates.current)) setVolumePaused(true);
      },
    );
  }, [
    client,
    namespace,
    where,
    sort,
    limit,
    linkLabels,
    queryKey,
    nonce,
    apply,
  ]);

  // Resume (or the first result) applies whatever arrived while held.
  useEffect(() => {
    if (!live) return;
    const result = latest.current;
    if (result === null) return;
    apply(result);
  }, [live, apply]);

  const togglePause = useCallback(() => {
    if (volumePaused) {
      updates.current = [];
      setVolumePaused(false);
      setPaused(false);
      return;
    }
    setPaused((current) => !current);
  }, [volumePaused]);

  const resume = useCallback(() => {
    updates.current = [];
    setPaused(false);
    setVolumePaused(false);
  }, []);

  const error =
    snapshot !== null && snapshot.key === queryKey ? snapshot.error : null;

  const refresh = useCallback(() => {
    const result = latest.current;
    if (error !== null || result === null) {
      setSnapshot(null);
      setNonce((current) => current + 1);
      return;
    }
    apply(result);
  }, [apply, error]);

  const current =
    snapshot !== null && snapshot.key === queryKey ? snapshot : null;

  return {
    rows: current?.rows ?? [],
    hasNextPage: current?.hasNextPage ?? false,
    isLoading: current === null && client !== null && namespace !== null,
    error,
    pending: held !== null && held.key === queryKey,
    paused: !live,
    volumePaused,
    togglePause,
    resume,
    refresh,
  };
}
