import { z } from 'zod';

/**
 * Unsaved entity-sheet edits (C.6). A draft is per entity and per tab: closing
 * the tab throws it away, while reopening the sheet in the same tab offers to
 * restore it. Drafts never leave the browser and are cleared on save,
 * discard and cancel.
 */

export const DRAFT_PREFIX = 'br.draft.';

/** Stand-in id for the sheet opened with `?new=1`. */
export const NEW_ENTITY_DRAFT_ID = 'new';

const primitiveSchema = z.union([
  z.string(),
  z.number(),
  z.boolean(),
  z.null(),
]);

const linkDraftSchema = z.object({
  add: z.array(z.string()),
  remove: z.array(z.string()),
});

export const explorerDraftSchema = z.object({
  values: z.record(z.string(), primitiveSchema),
  unset: z.array(z.string()),
  json: z.record(z.string(), z.string()),
  links: z.record(z.string(), linkDraftSchema),
});

export type ExplorerDraft = z.infer<typeof explorerDraftSchema>;

export function draftKey(
  appId: string,
  namespace: string,
  entityId: string,
): string {
  return `${DRAFT_PREFIX}${appId}.${namespace}.${entityId}`;
}

/** Reads a draft; an unreadable or outdated entry behaves like no draft. */
export function readDraft(
  appId: string,
  namespace: string,
  entityId: string,
  storage: Storage | null = browserSessionStorage(),
): ExplorerDraft | null {
  if (storage === null) return null;
  try {
    const raw = storage.getItem(draftKey(appId, namespace, entityId));
    if (raw === null) return null;
    const parsed = explorerDraftSchema.safeParse(JSON.parse(raw));
    return parsed.success ? parsed.data : null;
  } catch {
    return null;
  }
}

export function writeDraft(
  appId: string,
  namespace: string,
  entityId: string,
  draft: ExplorerDraft,
  storage: Storage | null = browserSessionStorage(),
): void {
  if (storage === null) return;
  try {
    storage.setItem(
      draftKey(appId, namespace, entityId),
      JSON.stringify(draft),
    );
  } catch {
    /* The sheet still works; only the recovery offer is lost. */
  }
}

export function clearDraft(
  appId: string,
  namespace: string,
  entityId: string,
  storage: Storage | null = browserSessionStorage(),
): void {
  if (storage === null) return;
  try {
    storage.removeItem(draftKey(appId, namespace, entityId));
  } catch {
    /* Nothing to clean up when storage is blocked. */
  }
}

export function emptyDraft(): ExplorerDraft {
  return { values: {}, unset: [], json: {}, links: {} };
}

function browserSessionStorage(): Storage | null {
  if (typeof window === 'undefined') return null;
  try {
    return window.sessionStorage;
  } catch {
    return null;
  }
}
