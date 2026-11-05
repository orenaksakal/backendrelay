import {
  SimpleTable,
  type SimpleTableColumn,
} from '@/components/data/simple-table';
import { Badge } from '@/components/ui/badge';
import type { PermissionsCheckRow } from '@/lib/permissions/check';

/**
 * The C.10/C.12 permission-check table. Query and transaction checks come from
 * the same normalizer, so the Playground reuses the columns the Test-as sheet
 * shows; `key` keeps duplicate checks apart.
 */

type CheckRow = PermissionsCheckRow & { key: string };

function checkRows(checks: ReadonlyArray<PermissionsCheckRow>): CheckRow[] {
  return checks.map((row, index) => ({
    ...row,
    key: [
      row.namespace,
      row.entityId ?? 'none',
      row.action,
      String(index),
    ].join('-'),
  }));
}

const columns: ReadonlyArray<SimpleTableColumn<CheckRow>> = [
  {
    id: 'namespace',
    header: 'Namespace',
    cell: (row) => <span className="font-mono text-xs">{row.namespace}</span>,
  },
  {
    id: 'entity',
    header: 'Entity',
    cell: (row) => (
      <span className="font-mono text-xs">{row.entityId ?? '—'}</span>
    ),
  },
  {
    id: 'action',
    header: 'Action',
    cell: (row) => row.action,
  },
  {
    id: 'expression',
    header: 'Expression',
    cell: (row) => (
      <span className="font-mono text-xs break-all">
        {row.expression === '' ? '—' : row.expression}
      </span>
    ),
  },
  {
    id: 'result',
    header: 'Result',
    cell: (row) => (
      <Badge tone={row.allowed ? 'success' : 'destructive'}>
        {row.allowed ? '✓ allowed' : '✗ denied'}
      </Badge>
    ),
  },
];

export function ChecksTable({
  checks,
}: {
  checks: ReadonlyArray<PermissionsCheckRow>;
}) {
  return (
    <SimpleTable
      caption="Permission checks"
      columns={columns}
      rows={checkRows(checks)}
      getRowId={(row) => row.key}
    />
  );
}
