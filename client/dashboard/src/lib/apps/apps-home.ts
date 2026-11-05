import { z } from 'zod';
import type { AppSummary } from '@/lib/api/endpoints/apps';
import type { App } from '@/lib/api/endpoints/schemas';
import {
  DEFAULT_PAGE_SIZE,
  isPageSize,
  type PageSize,
} from '@/lib/account/preferences';

/**
 * Apps home model (C.4): the search URL contract, the row shape that joins one
 * app with its D0.2 summary, and the filter, sort and page-size rules.
 */

export const APPS_SORT_IDS = [
  'name',
  'sessions',
  'data',
  'files',
  'last-write',
  'last-backup',
] as const;

export type AppsSortId = (typeof APPS_SORT_IDS)[number];

export const SORT_DIRECTIONS = ['asc', 'desc'] as const;
export type SortDirection = (typeof SORT_DIRECTIONS)[number];

export const APP_STATUS_FILTERS = [
  'all',
  'active',
  'read-only',
  'disabled',
] as const;

export type AppStatusFilter = (typeof APP_STATUS_FILTERS)[number];

export const DEFAULT_APPS_SORT: AppsSortId = 'last-write';
export const DEFAULT_APPS_DIRECTION: SortDirection = 'desc';

export const APPS_SORT_LABELS: Record<AppsSortId, string> = {
  name: 'Name',
  sessions: 'Sessions',
  data: 'Data',
  files: 'Files',
  'last-write': 'Last activity',
  'last-backup': 'Last backup',
};

export const APP_STATUS_FILTER_LABELS: Record<AppStatusFilter, string> = {
  all: 'All',
  active: 'Active',
  'read-only': 'Read-only',
  disabled: 'Disabled',
};

/** `?sort=`/`?dir=` stay readable and fall back instead of failing the route. */
export const appsHomeSearchSchema = z.object({
  sort: z.enum(APPS_SORT_IDS).catch(DEFAULT_APPS_SORT),
  dir: z.enum(SORT_DIRECTIONS).catch(DEFAULT_APPS_DIRECTION),
});

export type AppsHomeSearch = z.infer<typeof appsHomeSearchSchema>;

export function isAppsSortId(value: unknown): value is AppsSortId {
  return (
    typeof value === 'string' &&
    (APPS_SORT_IDS as readonly string[]).includes(value)
  );
}

export type AppStatus = NonNullable<App['status']>;

/**
 * The server sends `status`; `effective_status` wins when a caller computes it
 * (the old dashboard did). A missing status is an active app.
 */
export function effectiveAppStatus(
  app: Pick<App, 'status' | 'effective_status'>,
): AppStatus {
  return app.effective_status ?? app.status ?? 'active';
}

export interface AppsHomeRow {
  app: App;
  /** Absent only while the summary query has no entry for this app. */
  metrics: AppSummary | undefined;
  pinned: boolean;
}

/** Joins the workspace app list with the one-query summary (C.4). */
export function buildAppsHomeRows(
  apps: readonly App[],
  summaries: readonly AppSummary[],
  pinnedIds: readonly string[],
): AppsHomeRow[] {
  const byId = new Map<string, AppSummary>(
    summaries.map((summary) => [summary.id, summary]),
  );
  const pinned = new Set(pinnedIds);
  return apps.map((app) => ({
    app,
    metrics: byId.get(app.id),
    pinned: pinned.has(app.id),
  }));
}

/** Search matches the title and the ID; the status filter is exact. */
export function filterAppsHomeRows(
  rows: readonly AppsHomeRow[],
  query: string,
  status: AppStatusFilter,
): AppsHomeRow[] {
  const needle = query.trim().toLowerCase();
  return rows.filter((row) => {
    if (status !== 'all' && effectiveAppStatus(row.app) !== status)
      return false;
    if (needle === '') return true;
    return (
      row.app.title.toLowerCase().includes(needle) ||
      row.app.id.toLowerCase().includes(needle)
    );
  });
}

function sortValue(row: AppsHomeRow, sort: AppsSortId): string | number | null {
  switch (sort) {
    case 'name':
      return row.app.title;
    case 'sessions':
      return row.metrics?.liveSessions ?? null;
    case 'data':
      return row.metrics?.appBytes ?? null;
    case 'files':
      return row.metrics?.storageBytes ?? null;
    case 'last-write':
      return row.metrics?.lastWriteAt ?? null;
    case 'last-backup':
      return row.metrics?.lastBackupAt ?? null;
  }
}

/** Pinned apps always come first; missing metrics sort last in either direction. */
export function sortAppsHomeRows(
  rows: readonly AppsHomeRow[],
  sort: AppsSortId,
  dir: SortDirection,
): AppsHomeRow[] {
  const factor = dir === 'asc' ? 1 : -1;
  return [...rows].sort((left, right) => {
    if (left.pinned !== right.pinned) return left.pinned ? -1 : 1;

    const a = sortValue(left, sort);
    const b = sortValue(right, sort);
    if (a === null && b === null) {
      return left.app.title.localeCompare(right.app.title);
    }
    if (a === null) return 1;
    if (b === null) return -1;

    const compared =
      typeof a === 'number' && typeof b === 'number'
        ? a - b
        : String(a).localeCompare(String(b), undefined, {
            numeric: true,
            sensitivity: 'base',
          });
    if (compared !== 0) return compared * factor;
    return left.app.title.localeCompare(right.app.title);
  });
}

/** The warning icon shows when the last backup is missing or older than 7 days. */
export const BACKUP_STALE_MS = 7 * 24 * 60 * 60 * 1000;

export function backupIsStale(
  lastBackupAt: string | null | undefined,
  now: number = Date.now(),
): boolean {
  if (lastBackupAt === null || lastBackupAt === undefined) return true;
  const timestamp = Date.parse(lastBackupAt);
  return !Number.isFinite(timestamp) || now - timestamp > BACKUP_STALE_MS;
}

/** C.4 remembers the apps table page size separately from the account default. */
export const APPS_PAGE_SIZE_STORAGE_KEY = 'br.pageSize.apps';

export function readAppsPageSize(
  storage: Storage | null = storageOrNull(),
): PageSize {
  if (storage === null) return DEFAULT_PAGE_SIZE;
  const parsed = Number(readKey(storage, APPS_PAGE_SIZE_STORAGE_KEY));
  return isPageSize(parsed) ? parsed : DEFAULT_PAGE_SIZE;
}

export function writeAppsPageSize(
  value: PageSize,
  storage: Storage | null = storageOrNull(),
): void {
  if (storage === null) return;
  try {
    storage.setItem(APPS_PAGE_SIZE_STORAGE_KEY, String(value));
  } catch {
    /* The table still pages for this session. */
  }
}

function storageOrNull(): Storage | null {
  if (typeof window === 'undefined') return null;
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

function readKey(storage: Storage, key: string): string | null {
  try {
    return storage.getItem(key);
  } catch {
    return null;
  }
}
