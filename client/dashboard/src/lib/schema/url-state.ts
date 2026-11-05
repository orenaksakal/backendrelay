import { z } from 'zod';

/**
 * Schema URL state (C.7, B.6): `ns` selects the namespace and `tab` the view.
 * Both live in the URL so a copied link restores the screen and the browser's
 * back/forward buttons walk the tabs.
 */

export const SCHEMA_TABS = [
  'attributes',
  'links',
  'code',
  'jobs',
  'deleted',
] as const;

export type SchemaTab = (typeof SCHEMA_TABS)[number];

export const SCHEMA_TAB_LABELS: Record<SchemaTab, string> = {
  attributes: 'Attributes',
  links: 'Links',
  code: 'Code',
  jobs: 'Jobs',
  deleted: 'Deleted',
};

export function isSchemaTab(value: unknown): value is SchemaTab {
  return SCHEMA_TABS.some((tab) => tab === value);
}

/** A hand-edited URL falls back to the default instead of failing the route. */
export const schemaSearchSchema = z.object({
  ns: z.string().trim().min(1).optional().catch(undefined),
  tab: z.enum(SCHEMA_TABS).catch('attributes'),
});

export type SchemaSearch = z.infer<typeof schemaSearchSchema>;

export function schemaSearchFor(
  namespace: string | null,
  tab: SchemaTab,
): SchemaSearch {
  return {
    ns: namespace ?? undefined,
    tab,
  };
}

/** The URL form keeps the selected namespace while switching tabs. */
export function schemaTabSearch(
  search: SchemaSearch,
  tab: SchemaTab,
): SchemaSearch {
  return { ns: search.ns, tab };
}
