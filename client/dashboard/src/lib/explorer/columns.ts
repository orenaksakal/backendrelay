import type { AppAttribute, AppLink, AppNamespace } from '@/lib/instant';

/**
 * Grid columns for one namespace (C.6): `id` first (pinned left), then the
 * attributes in schema order, then one column per link. The descriptor is the
 * stable column identity used by the columns menu and its localStorage entry.
 */

export type ExplorerColumn =
  | { kind: 'attribute'; id: string; attribute: AppAttribute }
  | { kind: 'link'; id: string; link: AppLink };

export function explorerColumns(
  namespace: AppNamespace | null,
): ExplorerColumn[] {
  if (namespace === null) return [];
  return [
    ...namespace.attributes.map((attribute) => ({
      kind: 'attribute' as const,
      id: attribute.name,
      attribute,
    })),
    ...namespace.links.map((link) => ({
      kind: 'link' as const,
      id: `link:${link.id}`,
      link,
    })),
  ];
}

export function columnLabel(column: ExplorerColumn): string {
  return column.kind === 'attribute'
    ? column.attribute.name
    : column.link.label;
}

/** The `id` column identifies a row, so it is never hidden. */
export function isHideableColumn(column: ExplorerColumn): boolean {
  return !(column.kind === 'attribute' && column.attribute.name === 'id');
}

export function visibleColumns(
  columns: ReadonlyArray<ExplorerColumn>,
  hidden: ReadonlyArray<string>,
): ExplorerColumn[] {
  const hiddenIds = new Set(hidden);
  return columns.filter((column) => !hiddenIds.has(column.id));
}
