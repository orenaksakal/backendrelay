import { useMemo, useState } from 'react';
import {
  SimpleTable,
  type SimpleTableColumn,
} from '@/components/data/simple-table';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
import type { AppSchema } from '@/lib/instant';
import { editorNamespaces } from '@/lib/permissions/document';
import {
  PERMISSION_ACTION_LABELS,
  PERMISSION_ACTIONS,
  type PermissionAction,
} from '@/lib/permissions/effective';
import {
  DEFAULT_OVERVIEW_FILTERS,
  filterPermissionOverviewRows,
  permissionOverviewRows,
  type PermissionOverviewCell,
  type PermissionOverviewRow,
} from '@/lib/permissions/overview';
import type { Rules } from '@/lib/permissions/model';

export interface PermissionsOverviewProps {
  rules: Rules;
  schema: AppSchema | undefined;
  /** Opens the Editor on a namespace, optionally focused on one action. */
  onOpen: (namespace: string, action?: PermissionAction) => void;
}

/**
 * C.10 Overview: one row per namespace and one column per object action, with
 * the same effective-state wording as the editor. Cells open the editor.
 */
export function PermissionsOverview({
  rules,
  schema,
  onOpen,
}: PermissionsOverviewProps) {
  const [filters, setFilters] = useState(DEFAULT_OVERVIEW_FILTERS);

  const rows = useMemo(() => {
    const namespaces = editorNamespaces(
      rules,
      schema?.namespaces.map((namespace) => namespace.name) ?? [],
    );
    return filterPermissionOverviewRows(
      permissionOverviewRows(rules, namespaces),
      filters,
    );
  }, [rules, schema, filters]);

  const columns: ReadonlyArray<SimpleTableColumn<PermissionOverviewRow>> = [
    {
      id: 'namespace',
      header: 'Namespace',
      cell: (row) => (
        <button
          type="button"
          className="hover:text-accent min-h-6 w-full text-left font-mono text-sm"
          aria-label={`Open ${row.namespace} rules`}
          onClick={() => {
            onOpen(row.namespace);
          }}
        >
          {row.namespace}
          {row.publicWrites ? (
            <span className="text-warning ml-1" aria-hidden="true">
              ⚠
            </span>
          ) : null}
        </button>
      ),
    },
    ...PERMISSION_ACTIONS.map(
      (action): SimpleTableColumn<PermissionOverviewRow> => ({
        id: action,
        header: PERMISSION_ACTION_LABELS[action],
        cell: (row) => (
          <OverviewCell
            namespace={row.namespace}
            action={action}
            cell={row.actions[action]}
            onOpen={onOpen}
          />
        ),
      }),
    ),
    {
      id: 'link',
      header: 'Link',
      cell: (row) => (
        <button
          type="button"
          className="min-h-6 w-full text-left"
          aria-label={`Open ${row.namespace} link rules`}
          onClick={() => {
            onOpen(row.namespace);
          }}
        >
          {row.linkRules === 0 ? (
            <span className="text-muted-foreground">—</span>
          ) : (
            <Badge tone="neutral">
              {row.linkRules === 1
                ? '1 rule'
                : `${String(row.linkRules)} rules`}
            </Badge>
          )}
        </button>
      ),
    },
  ];

  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center gap-4">
        <div className="flex items-center gap-2 text-sm">
          <Checkbox
            aria-label="Only public writes"
            checked={filters.onlyPublicWrites}
            onCheckedChange={(checked) => {
              setFilters((current) => ({
                ...current,
                onlyPublicWrites: checked,
              }));
            }}
          />
          <span>Only public writes</span>
        </div>
        <div className="flex items-center gap-2 text-sm">
          <Checkbox
            aria-label="Hide system namespaces"
            checked={filters.hideSystemNamespaces}
            onCheckedChange={(checked) => {
              setFilters((current) => ({
                ...current,
                hideSystemNamespaces: checked,
              }));
            }}
          />
          <span>Hide system namespaces</span>
        </div>
      </div>

      <SimpleTable
        caption="Permission overview"
        columns={columns}
        rows={rows}
        getRowId={(row) => row.namespace}
        onRowActivate={(row) => {
          onOpen(row.namespace);
        }}
        empty={
          <span className="inline-flex flex-wrap items-center justify-center gap-1">
            No namespaces match these filters.
            <Button
              type="button"
              variant="link"
              size="sm"
              onClick={() => {
                setFilters(DEFAULT_OVERVIEW_FILTERS);
              }}
            >
              Clear filters
            </Button>
          </span>
        }
      />
    </div>
  );
}

interface OverviewCellProps {
  namespace: string;
  action: PermissionAction;
  cell: PermissionOverviewCell;
  onOpen: PermissionsOverviewProps['onOpen'];
}

function OverviewCell({ namespace, action, cell, onOpen }: OverviewCellProps) {
  const label = `${namespace} ${action} rule`;
  const isExpression =
    cell.state === 'custom' || cell.state === 'system-default';

  return (
    <button
      type="button"
      className="min-h-6 w-full max-w-56 text-left"
      aria-label={`Open ${label}`}
      title={cell.expression}
      onClick={() => {
        onOpen(namespace, action);
      }}
    >
      {isExpression ? (
        <span className="text-cell block truncate font-mono">
          {cell.expression}
        </span>
      ) : (
        <Badge tone={cell.tone} title={cell.description}>
          {cell.label}
        </Badge>
      )}
    </button>
  );
}
