import { z } from 'zod';

/**
 * Explorer preferences that outlive a visit but never leave the browser (C.6):
 * pinned namespaces per app and hidden columns per app namespace. Both keys
 * match the plan's storage names; a blocked localStorage only costs the
 * preference, never the screen.
 */

export const NAMESPACE_PINS_PREFIX = 'br.nsPins.';
export const HIDDEN_COLUMNS_PREFIX = 'br.cols.';

const storedIdsSchema = z.array(z.string().min(1));

export function namespacePinsKey(appId: string): string {
  return `${NAMESPACE_PINS_PREFIX}${appId}`;
}

export function hiddenColumnsKey(appId: string, namespace: string): string {
  return `${HIDDEN_COLUMNS_PREFIX}${appId}.${namespace}`;
}

function readIds(key: string, storage: Storage | null): string[] {
  if (storage === null) return [];
  try {
    const raw = storage.getItem(key);
    if (raw === null) return [];
    const parsed = storedIdsSchema.safeParse(JSON.parse(raw));
    return parsed.success ? parsed.data : [];
  } catch {
    return [];
  }
}

function writeIds(
  key: string,
  ids: readonly string[],
  storage: Storage | null,
): void {
  if (storage === null) return;
  try {
    storage.setItem(key, JSON.stringify(ids));
  } catch {
    /* The preference lasts this session when storage is blocked. */
  }
}

export function readNamespacePins(
  appId: string,
  storage: Storage | null = browserStorage(),
): string[] {
  return readIds(namespacePinsKey(appId), storage);
}

export function writeNamespacePins(
  appId: string,
  namespaces: readonly string[],
  storage: Storage | null = browserStorage(),
): void {
  writeIds(namespacePinsKey(appId), namespaces, storage);
}

/** Toggles one namespace and returns the new pinned list. */
export function toggleNamespacePin(
  appId: string,
  namespace: string,
  storage: Storage | null = browserStorage(),
): string[] {
  const current = readNamespacePins(appId, storage);
  const next = current.includes(namespace)
    ? current.filter((candidate) => candidate !== namespace)
    : [...current, namespace];
  writeNamespacePins(appId, next, storage);
  return next;
}

export function readHiddenColumns(
  appId: string,
  namespace: string,
  storage: Storage | null = browserStorage(),
): string[] {
  return readIds(hiddenColumnsKey(appId, namespace), storage);
}

export function writeHiddenColumns(
  appId: string,
  namespace: string,
  columns: readonly string[],
  storage: Storage | null = browserStorage(),
): void {
  writeIds(hiddenColumnsKey(appId, namespace), columns, storage);
}

/** Toggles one column and returns the new hidden list. */
export function toggleHiddenColumn(
  appId: string,
  namespace: string,
  columnId: string,
  storage: Storage | null = browserStorage(),
): string[] {
  const current = readHiddenColumns(appId, namespace, storage);
  const next = current.includes(columnId)
    ? current.filter((candidate) => candidate !== columnId)
    : [...current, columnId];
  writeHiddenColumns(appId, namespace, next, storage);
  return next;
}

function browserStorage(): Storage | null {
  if (typeof window === 'undefined') return null;
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}
