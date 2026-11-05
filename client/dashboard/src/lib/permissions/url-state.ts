import { z } from 'zod';

/**
 * Permissions URL state (C.10, B.6): `?view=` selects Editor, JSON, Overview or
 * History, `?ns=` opens the Editor on one namespace, and `?test=1` opens the
 * Test-as sheet. Hand-edited values fall back to the default instead of
 * failing the route.
 */

export const PERMISSION_VIEWS = [
  'editor',
  'json',
  'overview',
  'history',
] as const;

export type PermissionView = (typeof PERMISSION_VIEWS)[number];

export const PERMISSION_VIEW_LABELS: Record<PermissionView, string> = {
  editor: 'Editor',
  json: 'JSON',
  overview: 'Overview',
  history: 'History',
};

export const permissionsSearchSchema = z.object({
  view: z.enum(PERMISSION_VIEWS).catch('editor'),
  ns: z.string().trim().min(1).optional().catch(undefined),
  test: z
    .union([z.literal('1'), z.literal(1)])
    .optional()
    .catch(undefined),
});

export type PermissionsSearch = z.infer<typeof permissionsSearchSchema>;

export function permissionsViewSearch(view: PermissionView): PermissionsSearch {
  return { view };
}

/** `?test=1` opens the Test-as sheet; closing it drops the parameter. */
export function permissionsTestSearch(
  search: PermissionsSearch,
  open: boolean,
): PermissionsSearch {
  const next = { ...search };
  if (open) next.test = '1';
  else delete next.test;
  return next;
}
