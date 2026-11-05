import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  holdsLiveUpdates,
  isHighVolume,
  recordUpdate,
} from '@/lib/explorer/live';
import type { AppInstantClient } from '@/lib/instant';
import {
  buildFileQuery,
  buildFilesQuery,
  buildFilesWhere,
  FILES_PAGE_SIZE,
  parseFilesError,
  parseFilesResult,
  type FileRow,
  type FilesQueryResult,
} from './storage';

/**
 * Live `$files` data for C.9. The list follows B.5.5: updates apply in place,
 * a search keeps new results behind Refresh so rows never move under the
 * cursor, and a burst above ten updates per second pauses live data.
 */

interface FilesSnapshot extends FilesQueryResult {
  key: string;
  error: string | null;
}

export interface FilesList {
  files: FileRow[];
  hasNextPage: boolean;
  isLoading: boolean;
  error: string | null;
  /** A newer result is waiting behind Refresh (searching or paused). */
  pending: boolean;
  paused: boolean;
  /** Paused automatically because updates exceeded the live threshold. */
  volumePaused: boolean;
  togglePause: () => void;
  resume: () => void;
  refresh: () => void;
}

export interface UseFilesOptions {
  client: AppInstantClient | null;
  prefix: string;
  /** Only affects whether live results wait behind Refresh. */
  text?: string;
  limit?: number;
}

export function useFiles({
  client,
  prefix,
  text = '',
  limit = FILES_PAGE_SIZE,
}: UseFilesOptions): FilesList {
  const where = useMemo(() => buildFilesWhere(prefix), [prefix]);
  const queryKey = useMemo(
    () => JSON.stringify({ where, limit }),
    [where, limit],
  );
  const filtered = text.trim() !== '';

  const [snapshot, setSnapshot] = useState<FilesSnapshot | null>(null);
  const [held, setHeld] = useState<{
    key: string;
    result: FilesQueryResult;
  } | null>(null);
  const [paused, setPaused] = useState(false);
  const [volumePaused, setVolumePaused] = useState(false);
  const [nonce, setNonce] = useState(0);
  const latest = useRef<FilesQueryResult | null>(null);
  const hasData = useRef(false);
  const lastKey = useRef('');
  const updates = useRef<number[]>([]);
  const live = !paused && !volumePaused;
  const liveRef = useRef(true);

  const apply = useCallback(
    (result: FilesQueryResult) => {
      setSnapshot({
        key: queryKey,
        files: result.files,
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
    // A new folder starts from an empty list; a retry (`nonce`) keeps the rows
    // already on screen.
    if (lastKey.current !== queryKey) {
      lastKey.current = queryKey;
      latest.current = null;
      hasData.current = false;
      updates.current = [];
    }

    return client.core.subscribeQuery(
      buildFilesQuery({ prefix, limit }),
      (result) => {
        const failure = parseFilesError(result);
        if (failure !== null) {
          setSnapshot({
            key: queryKey,
            files: [],
            hasNextPage: false,
            error: failure,
          });
          setHeld(null);
          return;
        }

        const parsed = parseFilesResult(result);
        latest.current = parsed;
        updates.current = recordUpdate(updates.current, Date.now());

        // Pausing governs following changes, never the first page of a new
        // folder: navigating still shows its files while paused.
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
  }, [client, prefix, limit, filtered, queryKey, nonce, apply]);

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
    files: current?.files ?? [],
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

export interface FileDetail {
  file: FileRow | null;
  isLoading: boolean;
  error: string | null;
}

interface FileSnapshot {
  key: string;
  file: FileRow | null;
  error: string | null;
}

/**
 * One file by id for the sheet (C.9). Subscribing rather than reading the list
 * keeps a copied `?file=` link working and refreshes an expiring URL.
 */
export function useFile(
  client: AppInstantClient | null,
  fileId: string | null,
): FileDetail {
  const key = JSON.stringify({ fileId });
  const [snapshot, setSnapshot] = useState<FileSnapshot | null>(null);

  useEffect(() => {
    if (client === null || fileId === null) return;
    return client.core.subscribeQuery(buildFileQuery(fileId), (result) => {
      const failure = parseFilesError(result);
      if (failure !== null) {
        setSnapshot({ key, file: null, error: failure });
        return;
      }
      // A subscription can report loading before it has the row; a missing
      // row must not look like "this file was deleted".
      const payload =
        result !== null && typeof result === 'object'
          ? (result as Record<string, unknown>)
          : {};
      const data = payload['data'];
      const page =
        data !== null && typeof data === 'object'
          ? (data as Record<string, unknown>)['$files']
          : undefined;
      if (!Array.isArray(page)) return;
      const parsed = parseFilesResult(result);
      setSnapshot({ key, file: parsed.files[0] ?? null, error: null });
    });
  }, [client, fileId, key]);

  const current = snapshot !== null && snapshot.key === key ? snapshot : null;

  return {
    file: current?.file ?? null,
    error: current?.error ?? null,
    isLoading: current === null && client !== null && fileId !== null,
  };
}
