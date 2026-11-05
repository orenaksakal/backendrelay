import { z } from 'zod';
import { PERSONAL_WORKSPACE_ID } from '@/lib/workspace/workspaces';

/**
 * Palette ordering memory (B.7): the last five apps opened and the pinned apps
 * the apps home writes. Both live in localStorage under the keys the previous
 * dashboard used, so an upgrade keeps them. Storage failures are swallowed: the
 * palette still opens, just without the extra ordering.
 */

export const RECENT_APPS_STORAGE_KEY = 'br.recentApps';
export const RECENT_APP_LIMIT = 5;

export function readRecentAppIds(
  storage: Storage | null = browserStorage(),
): string[] {
  return readIds(RECENT_APPS_STORAGE_KEY, storage).slice(0, RECENT_APP_LIMIT);
}

export function rememberRecentApp(
  appId: string,
  storage: Storage | null = browserStorage(),
): void {
  if (storage === null) return;
  const ids = [
    appId,
    ...readRecentAppIds(storage).filter((candidate) => candidate !== appId),
  ].slice(0, RECENT_APP_LIMIT);
  try {
    storage.setItem(RECENT_APPS_STORAGE_KEY, JSON.stringify(ids));
  } catch {
    /* The ordering is a convenience, never a requirement. */
  }
}

/** Same list `www` wrote, kept readable so existing pins survive the upgrade. */
export function pinnedAppsKey(workspaceId: string): string {
  const effective = workspaceId === '' ? PERSONAL_WORKSPACE_ID : workspaceId;
  return `${effective}-pinned-apps`;
}

/** Apps home pins one list per workspace under the C.4 key. */
export function workspacePinsKey(workspaceId: string): string {
  const effective = workspaceId === '' ? PERSONAL_WORKSPACE_ID : workspaceId;
  return `br.pins.${effective}`;
}

export function readWorkspacePinnedAppIds(
  workspaceId: string,
  storage: Storage | null = browserStorage(),
): string[] {
  return readIds(workspacePinsKey(workspaceId), storage);
}

export function writeWorkspacePinnedAppIds(
  workspaceId: string,
  appIds: readonly string[],
  storage: Storage | null = browserStorage(),
): void {
  if (storage === null) return;
  try {
    storage.setItem(workspacePinsKey(workspaceId), JSON.stringify(appIds));
  } catch {
    /* Pinning is a convenience, never a requirement. */
  }
}

/** Toggles one app and returns the new list; a failed write lasts this session. */
export function toggleWorkspacePinnedApp(
  workspaceId: string,
  appId: string,
  storage: Storage | null = browserStorage(),
): string[] {
  const current = readWorkspacePinnedAppIds(workspaceId, storage);
  const next = current.includes(appId)
    ? current.filter((candidate) => candidate !== appId)
    : [...current, appId];
  writeWorkspacePinnedAppIds(workspaceId, next, storage);
  return next;
}

/** The palette orders by every workspace's pins, old key and new key alike. */
export function readPinnedAppIds(
  workspaceIds: readonly string[],
  storage: Storage | null = browserStorage(),
): string[] {
  const ids = new Set<string>();
  for (const workspaceId of workspaceIds) {
    for (const id of readIds(workspacePinsKey(workspaceId), storage)) {
      ids.add(id);
    }
    for (const id of readIds(pinnedAppsKey(workspaceId), storage)) {
      ids.add(id);
    }
  }
  return [...ids];
}

const storedIdsSchema = z.array(z.string().min(1));

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

function browserStorage(): Storage | null {
  if (typeof window === 'undefined') return null;
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}
