import { useCallback, useState } from 'react';

/**
 * Account → Preferences (C.18). These are browser preferences, not server
 * state: they describe how this operator works in this browser.
 */

/** The page-size default for lists that offer 25/50/100/200 (B.4). */
export const PAGE_SIZE_STORAGE_KEY = 'br.pageSize';

export const DEFAULT_PAGE_SIZE = 50;

export const PAGE_SIZE_OPTIONS = [25, 50, 100, 200] as const;

export type PageSize = (typeof PAGE_SIZE_OPTIONS)[number];

export function isPageSize(value: unknown): value is PageSize {
  return (
    typeof value === 'number' &&
    (PAGE_SIZE_OPTIONS as readonly number[]).includes(value)
  );
}

/** An unknown or out-of-range value falls back to the 50-row default. */
export function readDefaultPageSize(
  storage: Storage | null = storageOrNull(),
): PageSize {
  if (storage === null) return DEFAULT_PAGE_SIZE;
  const parsed = Number(readKey(storage, PAGE_SIZE_STORAGE_KEY));
  return isPageSize(parsed) ? parsed : DEFAULT_PAGE_SIZE;
}

export function writeDefaultPageSize(
  value: PageSize,
  storage: Storage | null = storageOrNull(),
): void {
  writeKey(storage, PAGE_SIZE_STORAGE_KEY, String(value));
}

export interface AccountPreferences {
  defaultPageSize: PageSize;
  setDefaultPageSize: (value: PageSize) => void;
}

/** Preferences with a setter that persists the change immediately. */
export function useAccountPreferences(): AccountPreferences {
  const [defaultPageSize, setDefaultPageSizeState] = useState(() =>
    readDefaultPageSize(),
  );

  const setDefaultPageSize = useCallback((value: PageSize) => {
    setDefaultPageSizeState(value);
    writeDefaultPageSize(value);
  }, []);

  return {
    defaultPageSize,
    setDefaultPageSize,
  };
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

function writeKey(storage: Storage | null, key: string, value: string): void {
  if (storage === null) return;
  try {
    storage.setItem(key, value);
  } catch {
    /* The preference still applies for this session. */
  }
}
