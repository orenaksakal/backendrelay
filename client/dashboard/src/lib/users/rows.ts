import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  holdsLiveUpdates,
  isHighVolume,
  recordUpdate,
} from '@/lib/explorer/live';
import type { AppInstantClient } from '@/lib/instant';
import type { UsersType } from './url-state';
import {
  buildUserQuery,
  buildUsersQuery,
  buildUsersWhere,
  parseUsersError,
  parseUsersResult,
  type UserRow,
  type UsersQueryResult,
} from './users';

/**
 * Live `$users` data for C.8. The list follows B.5.5: updates apply in place,
 * but a filtered list holds them behind Refresh so rows never move under the
 * cursor, and a burst above ten updates per second pauses live data.
 */

interface UsersSnapshot extends UsersQueryResult {
  key: string;
  error: string | null;
}

export interface UsersList {
  users: UserRow[];
  hasNextPage: boolean;
  isLoading: boolean;
  error: string | null;
  /** A newer result is waiting behind Refresh (filtered or paused). */
  pending: boolean;
  paused: boolean;
  /** Paused automatically because updates exceeded the live threshold. */
  volumePaused: boolean;
  togglePause: () => void;
  resume: () => void;
  refresh: () => void;
}

export interface UseUsersOptions {
  client: AppInstantClient | null;
  text: string;
  type: UsersType;
  limit: number;
}

export function useUsers({
  client,
  text,
  type,
  limit,
}: UseUsersOptions): UsersList {
  const where = useMemo(() => buildUsersWhere({ text, type }), [text, type]);
  const queryKey = useMemo(
    () => JSON.stringify({ where, limit }),
    [where, limit],
  );
  const filtered = text.trim() !== '' || type !== 'all';

  const [snapshot, setSnapshot] = useState<UsersSnapshot | null>(null);
  const [held, setHeld] = useState<{
    key: string;
    result: UsersQueryResult;
  } | null>(null);
  const [paused, setPaused] = useState(false);
  const [volumePaused, setVolumePaused] = useState(false);
  const [nonce, setNonce] = useState(0);
  const latest = useRef<UsersQueryResult | null>(null);
  const hasData = useRef(false);
  const lastKey = useRef('');
  const updates = useRef<number[]>([]);
  const live = !paused && !volumePaused;
  const liveRef = useRef(true);

  const apply = useCallback(
    (result: UsersQueryResult) => {
      setSnapshot({
        key: queryKey,
        users: result.users,
        hasNextPage: result.hasNextPage,
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
    if (client === null) {
      latest.current = null;
      hasData.current = false;
      return;
    }
    // A new filter or page size starts from an empty list; a retry (`nonce`)
    // keeps the rows already on screen.
    if (lastKey.current !== queryKey) {
      lastKey.current = queryKey;
      latest.current = null;
      hasData.current = false;
      updates.current = [];
    }

    return client.core.subscribeQuery(
      buildUsersQuery({ where, limit }),
      (result) => {
        const failure = parseUsersError(result);
        if (failure !== null) {
          setSnapshot({
            key: queryKey,
            users: [],
            hasNextPage: false,
            error: failure,
          });
          setHeld(null);
          return;
        }

        const parsed = parseUsersResult(result);
        latest.current = parsed;
        updates.current = recordUpdate(updates.current, Date.now());

        // Pausing governs following changes, never the first page of a new
        // query: a filter change still shows its matches while paused.
        if (
          hasData.current &&
          (!liveRef.current || holdsLiveUpdates(false, filtered))
        ) {
          setHeld({ key: queryKey, result: parsed });
          return;
        }

        apply(parsed);
        if (isHighVolume(updates.current)) setVolumePaused(true);
      },
    );
  }, [client, where, limit, filtered, queryKey, nonce, apply]);

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
    users: current?.users ?? [],
    hasNextPage: current?.hasNextPage ?? false,
    isLoading: current === null && client !== null,
    error,
    pending: held !== null && held.key === queryKey,
    paused: !live,
    volumePaused,
    togglePause,
    resume,
    refresh,
  };
}

export interface UserDetail {
  user: UserRow | null;
  isLoading: boolean;
  error: string | null;
}

/** Stable empty list so the hook does not resubscribe on every render. */
const NO_LINKS: ReadonlyArray<string> = [];

interface UserSnapshot {
  key: string;
  user: UserRow | null;
  error: string | null;
}

/**
 * One user by id for the sheet (C.8). The sheet queries the links on demand,
 * so the list query stays cheap and the linked data arrives only when opened.
 */
export function useUser(
  client: AppInstantClient | null,
  userId: string | null,
  linkLabels: ReadonlyArray<string> = NO_LINKS,
): UserDetail {
  const key = JSON.stringify({ userId, linkLabels });
  const [snapshot, setSnapshot] = useState<UserSnapshot | null>(null);

  useEffect(() => {
    if (client === null || userId === null) return;
    return client.core.subscribeQuery(
      buildUserQuery(userId, linkLabels),
      (result) => {
        const failure = parseUsersError(result);
        if (failure !== null) {
          setSnapshot({ key, user: null, error: failure });
          return;
        }
        // A subscription can report loading before it has the row; a missing
        // row list must not look like "this user has no data".
        const payload =
          result !== null && typeof result === 'object'
            ? (result as Record<string, unknown>)
            : {};
        const data = payload['data'];
        const page =
          data !== null && typeof data === 'object'
            ? (data as Record<string, unknown>)['$users']
            : undefined;
        if (!Array.isArray(page)) return;
        const parsed = parseUsersResult(result);
        setSnapshot({ key, user: parsed.users[0] ?? null, error: null });
      },
    );
  }, [client, userId, key, linkLabels]);

  const current = snapshot !== null && snapshot.key === key ? snapshot : null;

  return {
    user: current?.user ?? null,
    error: current?.error ?? null,
    isLoading: current === null && client !== null && userId !== null,
  };
}
