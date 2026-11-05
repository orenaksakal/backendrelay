import { z } from 'zod';
import { normalizePrefix } from './storage';

/**
 * Storage URL state (C.9, B.6): `prefix` is the open folder, `q` searches the
 * paths below it and `file` keeps the open sheet in the URL so a copied link
 * restores it.
 */

/** A hand-edited URL falls back to the default instead of failing the route. */
export const storageSearchSchema = z.object({
  prefix: z.string().trim().optional().catch(undefined),
  q: z.string().trim().min(1).optional().catch(undefined),
  file: z.string().trim().min(1).optional().catch(undefined),
});

export type StorageSearch = z.infer<typeof storageSearchSchema>;

/** The URL form of a view state; empty values are dropped from the URL. */
export function storageSearchFor(state: {
  prefix?: string | undefined;
  q?: string | undefined;
  file?: string | null | undefined;
}): StorageSearch {
  const prefix = normalizePrefix(state.prefix);
  const text = state.q?.trim() ?? '';
  return {
    prefix: prefix === '' ? undefined : prefix,
    q: text === '' ? undefined : text,
    file: state.file ?? undefined,
  };
}
