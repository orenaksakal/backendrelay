import { z } from 'zod';

/**
 * Unsaved permission rules (C.10). Editor and JSON share one draft, and it is
 * kept per app so reopening the tab offers the same text back. Drafts never
 * leave the browser and are cleared on save, reset and discard.
 */

export const PERMISSION_DRAFT_PREFIX = 'br.perms.';

const storedDraftSchema = z.object({
  base: z.string(),
  draft: z.string(),
});

export interface RecoveredDraft {
  base: string;
  draft: string;
  recovered: boolean;
}

export function permissionDraftKey(appId: string): string {
  return `${PERMISSION_DRAFT_PREFIX}${appId}`;
}

/** An unreadable, corrupt or unchanged entry behaves like no draft at all. */
export function recoverDraft(
  stored: string | null,
  saved: string,
): RecoveredDraft {
  try {
    const parsed = storedDraftSchema.safeParse(JSON.parse(stored ?? 'null'));
    if (parsed.success && parsed.data.base !== parsed.data.draft) {
      return {
        base: parsed.data.base,
        draft: parsed.data.draft,
        recovered: true,
      };
    }
  } catch {
    /* Ignore malformed browser storage. */
  }
  return { base: saved, draft: saved, recovered: false };
}

export function readPermissionDraft(
  appId: string,
  saved: string,
  storage: Storage | null = browserLocalStorage(),
): RecoveredDraft {
  if (storage === null) return recoverDraft(null, saved);
  try {
    return recoverDraft(storage.getItem(permissionDraftKey(appId)), saved);
  } catch {
    return recoverDraft(null, saved);
  }
}

export function writePermissionDraft(
  appId: string,
  base: string,
  draft: string,
  storage: Storage | null = browserLocalStorage(),
): void {
  if (storage === null) return;
  try {
    storage.setItem(permissionDraftKey(appId), JSON.stringify({ base, draft }));
  } catch {
    /* The editor still works; only the recovery offer is lost. */
  }
}

export function clearPermissionDraft(
  appId: string,
  storage: Storage | null = browserLocalStorage(),
): void {
  if (storage === null) return;
  try {
    storage.removeItem(permissionDraftKey(appId));
  } catch {
    /* Nothing to clean up when storage is blocked. */
  }
}

function browserLocalStorage(): Storage | null {
  if (typeof window === 'undefined') return null;
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}
