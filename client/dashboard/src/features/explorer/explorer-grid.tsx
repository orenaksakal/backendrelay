import type { ColDef } from 'ag-grid-community';
import { Asterisk, Fingerprint, Zap } from 'lucide-react';
import {
  lazy,
  Suspense,
  useCallback,
  useMemo,
  type ComponentType,
  type ReactNode,
} from 'react';
import type { DataGridProps, GridSort } from '@/components/data-grid/data-grid';
import { IdChip } from '@/components/data/id-chip';
import { RelativeTime } from '@/components/data/relative-time';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import {
  Popover,
  PopoverContent,
  PopoverTitle,
  PopoverTrigger,
} from '@/components/ui/popover';
import { TableSkeleton } from '@/components/feedback/skeletons';
import type { ExplorerColumn } from '@/lib/explorer/columns';
import type { ExplorerRow } from '@/lib/explorer/rows';
import type { ExplorerSort } from '@/lib/explorer/url-state';
import { isInlineEditable } from '@/lib/explorer/writes';
import type { AppAttribute, AppLink } from '@/lib/instant';
import { LinksPopover } from './links-popover';

/**
 * AG Grid is loaded on demand so the rest of the dashboard never pays for it
 * (research §AG Grid); the Explorer is its only production entry point.
 */
const DataGrid = lazy(
  () => import('@/components/data-grid/data-grid'),
) as unknown as ComponentType<DataGridProps<ExplorerRow>>;

/** Dense 13px cells for every Explorer column (B.2). */
const EXPLORER_DEFAULT_COL_DEF: ColDef<ExplorerRow> = {
  resizable: true,
  cellClass: 'text-cell',
};

export interface ExplorerGridProps {
  appId: string;
  columns: ReadonlyArray<ExplorerColumn>;
  rows: ExplorerRow[];
  sort: ExplorerSort | null;
  loading: boolean;
  onSortChange: (sort: ExplorerSort | null) => void;
  /** Selected entity ids, for the bulk bar (C.6). */
  onSelectionChange: (ids: string[]) => void;
  /** Inline edit committed; the Explorer writes it through the app client. */
  onEditCell: (
    attribute: AppAttribute,
    row: ExplorerRow,
    value: unknown,
  ) => void;
  /** JSON cells open the entity sheet instead of editing in place. */
  onOpenEntity: (id: string) => void;
  /**
   * The SDK devtool shows the same grid without edit sheets (C.20), so its
   * cells never enter edit mode.
   */
  readOnly?: boolean;
}

/**
 * C.6 grid: `id` pinned first, attributes with type/index/unique/required
 * headers, link columns with the "N links" popover, and sort wired back into
 * the URL through the DataGrid wrapper.
 */
export function ExplorerGrid({
  appId,
  columns,
  rows,
  sort,
  loading,
  onSortChange,
  onSelectionChange,
  onEditCell,
  onOpenEntity,
  readOnly = false,
}: ExplorerGridProps) {
  const columnDefs = useMemo(
    () =>
      columns.map((column) => {
        const definition =
          column.kind === 'attribute'
            ? attributeColumn(column.id, column.attribute, sort)
            : linkColumn(column.id, column.link, appId);
        return readOnly
          ? { ...definition, editable: false, cellEditor: undefined }
          : definition;
      }),
    [appId, columns, sort, readOnly],
  );

  const handleSortChanged = useCallback(
    (next: GridSort | null) => {
      if (next === null) {
        onSortChange(null);
        return;
      }
      onSortChange({
        attribute: next.colId,
        direction: next.direction,
      });
    },
    [onSortChange],
  );

  const handleCellEdit = useCallback(
    (change: { row: ExplorerRow; colId: string; value: unknown }) => {
      const column = columns.find((candidate) => candidate.id === change.colId);
      if (column === undefined) return;
      if (column.kind === 'link') {
        if (typeof change.row.id === 'string') onOpenEntity(change.row.id);
        return;
      }
      if (column.attribute.type === 'json') {
        if (typeof change.row.id === 'string') onOpenEntity(change.row.id);
        return;
      }
      onEditCell(column.attribute, change.row, change.value);
    },
    [columns, onEditCell, onOpenEntity],
  );

  return (
    <Suspense fallback={<TableSkeleton rows={8} columns={columns.length} />}>
      <DataGrid
        aria-label="Entities"
        rowData={rows}
        columnDefs={columnDefs}
        loading={loading}
        getRowId={(row) => (typeof row.id === 'string' ? row.id : '')}
        defaultColDef={EXPLORER_DEFAULT_COL_DEF}
        className="h-full"
        onSortChanged={handleSortChanged}
        rowSelection={
          readOnly
            ? undefined
            : {
                mode: 'multiRow',
                checkboxes: true,
                headerCheckbox: true,
                enableClickSelection: false,
              }
        }
        onSelectionChanged={
          readOnly
            ? undefined
            : (selected) => {
                onSelectionChange(
                  selected
                    .map((row) => row.id)
                    .filter((id): id is string => typeof id === 'string'),
                );
              }
        }
        readOnlyEdit
        onCellEditRequest={handleCellEdit}
      />
    </Suspense>
  );
}

interface HeaderValues {
  displayName?: string;
  type?: string;
  indexed?: boolean;
  unique?: boolean;
  required?: boolean;
  link?: boolean;
}

interface HeaderParams extends HeaderValues {
  /** AG Grid also hands the configured params through this nested field. */
  headerComponentParams?: HeaderValues;
}

function attributeColumn(
  colId: string,
  attribute: AppAttribute,
  sort: ExplorerSort | null,
): ColDef<ExplorerRow> {
  const isId = attribute.name === 'id';
  return {
    colId,
    headerName: attribute.name,
    pinned: isId ? 'left' : undefined,
    sortable: true,
    sort: sort?.attribute === attribute.name ? sort.direction : undefined,
    // JSON cells enter edit mode so the commit can open the entity sheet.
    editable:
      !isId && (isInlineEditable(attribute) || attribute.type === 'json'),
    cellEditor:
      attribute.type === 'boolean'
        ? 'agCheckboxCellEditor'
        : attribute.type === 'number'
          ? 'agNumberCellEditor'
          : undefined,
    valueGetter: (params) => params.data?.[attribute.name],
    cellRenderer: (params: CellParams) =>
      isId ? (
        <IdValue value={params.value} />
      ) : (
        <AttributeValue attribute={attribute} value={params.value} />
      ),
    cellClass:
      attribute.type === 'number' ? 'text-right tabular-nums' : undefined,
    headerComponent: AttributeHeader,
    headerComponentParams: {
      displayName: attribute.name,
      type: attribute.type,
      indexed: attribute.isIndexed,
      unique: attribute.isUnique,
      required: attribute.isRequired,
    } satisfies HeaderParams,
  };
}

function linkColumn(
  colId: string,
  link: AppLink,
  appId: string,
): ColDef<ExplorerRow> {
  return {
    colId,
    headerName: link.label,
    sortable: false,
    // Enter on a link cell opens the entity sheet, where links are edited.
    editable: true,
    cellEditor: 'agTextCellEditor',
    valueGetter: (params) => params.data?.[link.label],
    cellRenderer: (params: CellParams) =>
      params.value === null || params.value === undefined ? (
        <Unset />
      ) : Array.isArray(params.value) && params.value.length === 0 ? (
        <span className="text-muted-foreground text-xs">none</span>
      ) : (
        <LinksPopover appId={appId} link={link} value={params.value} />
      ),
    headerComponent: LinkHeader,
    headerComponentParams: {
      displayName: link.label,
      link: true,
    } satisfies HeaderParams,
  };
}

interface CellParams {
  value?: unknown;
}

function AttributeValue({
  attribute,
  value,
}: {
  attribute: AppAttribute;
  value: unknown;
}) {
  if (value === undefined) return <Unset />;
  if (value === null) return <NullValue />;

  switch (attribute.type) {
    case 'number':
      return (
        <span className="tabular-nums">
          {typeof value === 'number' ? value : null}
        </span>
      );
    case 'boolean':
      return <span>{value === true ? 'true' : 'false'}</span>;
    case 'date':
      return typeof value === 'string' || typeof value === 'number' ? (
        <RelativeTime value={value} />
      ) : (
        <span>{typeof value === 'string' ? value : null}</span>
      );
    case 'json':
      return <JsonValue value={value} />;
    case 'string':
      return (
        <span className="font-mono break-all">
          {typeof value === 'string'
            ? `"${value}"`
            : typeof value === 'number' || typeof value === 'boolean'
              ? String(value)
              : null}
        </span>
      );
    default:
      return typeof value === 'object' ? (
        <JsonValue value={value} />
      ) : (
        <span>
          {typeof value === 'number' || typeof value === 'boolean'
            ? String(value)
            : null}
        </span>
      );
  }
}

function IdValue({ value }: { value: unknown }) {
  if (typeof value !== 'string' || value === '') return <Unset />;
  return <IdChip value={value} tabIndex={-1} />;
}

function Unset() {
  return <span className="text-muted-foreground text-xs italic">unset</span>;
}

function NullValue() {
  return <Badge tone="neutral">null</Badge>;
}

/** `{…}` / `[…]` preview; the full JSON opens in a popover (C.6). */
function JsonValue({ value }: { value: unknown }) {
  const preview = Array.isArray(value) ? '[…]' : '{…}';
  return (
    <Popover>
      <PopoverTrigger
        render={
          <Button
            type="button"
            variant="link"
            size="sm"
            tabIndex={-1}
            className="h-6 px-0 font-mono text-xs"
          />
        }
      >
        {preview}
      </PopoverTrigger>
      <PopoverContent className="w-96">
        <PopoverTitle>JSON</PopoverTitle>
        <pre className="text-cell mt-2 max-h-64 overflow-auto font-mono break-all whitespace-pre-wrap">
          {JSON.stringify(value, null, 2)}
        </pre>
      </PopoverContent>
    </Popover>
  );
}

/**
 * AG Grid renders custom React headers with the column params plus
 * `headerComponentParams`, which is how the type badge and flags arrive.
 */
function AttributeHeader(params: HeaderParams) {
  const values = params.headerComponentParams ?? params;
  const { displayName, type, indexed, unique, required } = values;
  return (
    <span className="flex min-w-0 items-center gap-1.5">
      <span className="truncate">{displayName}</span>
      {type === undefined ? null : (
        <Badge tone="neutral" aria-label={`Type ${type}`}>
          {type}
        </Badge>
      )}
      {indexed === true ? (
        <HeaderFlag label="Indexed" icon={<Zap aria-hidden="true" />} />
      ) : null}
      {unique === true ? (
        <HeaderFlag label="Unique" icon={<Fingerprint aria-hidden="true" />} />
      ) : null}
      {required === true ? (
        <HeaderFlag label="Required" icon={<Asterisk aria-hidden="true" />} />
      ) : null}
    </span>
  );
}

function LinkHeader(params: HeaderParams) {
  const displayName =
    params.headerComponentParams?.displayName ?? params.displayName;
  return (
    <span className="flex min-w-0 items-center gap-1.5">
      <span className="truncate">{displayName}</span>
      <Badge tone="accent">link</Badge>
    </span>
  );
}

function HeaderFlag({ label, icon }: { label: string; icon: ReactNode }) {
  return (
    <span
      role="img"
      aria-label={label}
      title={label}
      className="text-muted-foreground [&_svg]:size-3"
    >
      {icon}
    </span>
  );
}
