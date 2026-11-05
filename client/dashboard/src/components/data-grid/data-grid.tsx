import {
  AllCommunityModule,
  ModuleRegistry,
  themeQuartz,
  type CellEditRequestEvent,
  type CellKeyDownEvent,
  type ColDef,
  type GetRowIdParams,
  type GridOptions,
  type RowDoubleClickedEvent,
  type SelectionChangedEvent,
  type SortChangedEvent,
} from 'ag-grid-community';
import { AgGridReact } from 'ag-grid-react';
import type { ReactNode } from 'react';
import { useCallback, useMemo } from 'react';
import { copyAnnouncement, useCopyFeedback } from '@/lib/clipboard';
import { cn } from '@/lib/utils';

/**
 * AG Grid Community is registered here, in the module the Explorer lazy-loads,
 * so no other chunk pays for it (research §AG Grid).
 */
ModuleRegistry.registerModules([AllCommunityModule]);

/** Grid chrome reads the same CSS variables as the rest of the dashboard. */
const gridTheme = themeQuartz.withParams({
  backgroundColor: 'var(--card)',
  foregroundColor: 'var(--foreground)',
  accentColor: 'var(--ring)',
  borderColor: 'var(--border)',
  headerBackgroundColor: 'var(--muted)',
  headerTextColor: 'var(--muted-foreground)',
  headerFontSize: '12px',
  fontFamily: 'var(--font-sans)',
  fontSize: '13px',
  wrapperBorderRadius: '8px',
  cellHorizontalPadding: 12,
});

export interface DataGridProps<Row> {
  /** Required: the grid has no visible caption (B.4, WCAG 1.3.1). */
  'aria-label': string;
  rowData: Row[];
  columnDefs: ColDef<Row>[];
  getRowId?: (row: Row) => string;
  defaultColDef?: ColDef<Row>;
  loading?: boolean;
  /** Extra content rendered above the grid, for example a toolbar. */
  toolbar?: ReactNode;
  /** Header sort changes, so the screen can keep them in the URL (C.6). */
  onSortChanged?: (sort: GridSort | null) => void;
  /** Row-selection mode; the Explorer uses `multiRow` with checkboxes. */
  rowSelection?: GridOptions<Row>['rowSelection'];
  onSelectionChanged?: (rows: Row[]) => void;
  /**
   * Read-only edit mode: AG Grid asks the screen to commit the change, which
   * keeps the live query as the only source of truth (C.6 inline editing).
   */
  readOnlyEdit?: boolean;
  onCellEditRequest?: (change: GridCellEdit<Row>) => void;
  onRowDoubleClicked?: (row: Row) => void;
  className?: string;
}

export interface GridCellEdit<Row> {
  row: Row;
  colId: string;
  value: unknown;
}

/** One sorted column, as the Explorer stores it in `?sort=`/`?dir=`. */
export interface GridSort {
  colId: string;
  direction: 'asc' | 'desc';
}

function cellText(value: unknown): string {
  if (value === null || value === undefined) {
    return '';
  }
  if (typeof value === 'string') {
    return value;
  }
  if (typeof value === 'number' || typeof value === 'boolean') {
    return String(value);
  }
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return '[unserializable value]';
  }
}

/**
 * AG Grid wrapper for the Explorer (B.4, C.6). Community has no clipboard, so
 * ⌘C / Ctrl+C copies the focused cell through `onCellKeyDown`.
 */
export default function DataGrid<Row>({
  'aria-label': ariaLabel,
  rowData,
  columnDefs,
  getRowId,
  defaultColDef,
  loading = false,
  toolbar,
  onSortChanged,
  rowSelection,
  onSelectionChanged,
  readOnlyEdit = false,
  onCellEditRequest,
  onRowDoubleClicked,
  className,
}: DataGridProps<Row>) {
  const { status, copy } = useCopyFeedback();

  const resolvedDefaultColDef = useMemo<ColDef<Row>>(
    () => ({
      sortable: true,
      resizable: true,
      suppressHeaderMenuButton: true,
      ...defaultColDef,
    }),
    [defaultColDef],
  );

  const handleSortChanged = useCallback(
    (event: SortChangedEvent<Row>) => {
      const sorted = event.api
        .getColumnState()
        .find((column) => column.sort === 'asc' || column.sort === 'desc');
      if (sorted === undefined) {
        onSortChanged?.(null);
        return;
      }
      onSortChanged?.({
        colId: sorted.colId,
        direction: sorted.sort === 'desc' ? 'desc' : 'asc',
      });
    },
    [onSortChanged],
  );

  const handleCellKeyDown = useCallback(
    (event: CellKeyDownEvent<Row>) => {
      const keyboardEvent = event.event;
      if (!(keyboardEvent instanceof KeyboardEvent)) {
        return;
      }
      if (!(keyboardEvent.metaKey || keyboardEvent.ctrlKey)) {
        return;
      }
      if (keyboardEvent.key.toLowerCase() !== 'c') {
        return;
      }
      keyboardEvent.preventDefault();
      void copy(cellText(event.value));
    },
    [copy],
  );

  const handleCellEditRequest = useCallback(
    (event: CellEditRequestEvent<Row>) => {
      if (!onCellEditRequest) return;
      onCellEditRequest({
        row: event.data,
        colId: event.column.getColId(),
        value: event.newValue,
      });
    },
    [onCellEditRequest],
  );

  const handleSelectionChanged = useCallback(
    (event: SelectionChangedEvent<Row>) => {
      if (!onSelectionChanged) return;
      onSelectionChanged(
        event.api
          .getSelectedNodes()
          .map((node) => node.data)
          .filter((row): row is Row => row !== undefined),
      );
    },
    [onSelectionChanged],
  );

  const handleRowDoubleClicked = useCallback(
    (event: RowDoubleClickedEvent<Row>) => {
      if (event.data === undefined) return;
      onRowDoubleClicked?.(event.data);
    },
    [onRowDoubleClicked],
  );

  return (
    <section
      aria-label={ariaLabel}
      aria-busy={loading}
      className={cn('flex min-h-0 flex-col gap-2', className)}
    >
      {toolbar}
      <div className="min-h-0 flex-1">
        <AgGridReact<Row>
          theme={gridTheme}
          rowData={rowData}
          columnDefs={columnDefs}
          defaultColDef={resolvedDefaultColDef}
          getRowId={
            getRowId
              ? (params: GetRowIdParams<Row>) => getRowId(params.data)
              : undefined
          }
          loading={loading}
          rowHeight={34}
          headerHeight={36}
          animateRows={false}
          ensureDomOrder
          tabToNextCell={() => false}
          suppressMultiSort
          rowSelection={rowSelection}
          readOnlyEdit={readOnlyEdit}
          onSortChanged={onSortChanged ? handleSortChanged : undefined}
          onCellKeyDown={handleCellKeyDown}
          onCellEditRequest={
            onCellEditRequest ? handleCellEditRequest : undefined
          }
          onSelectionChanged={
            onSelectionChanged ? handleSelectionChanged : undefined
          }
          onRowDoubleClicked={
            onRowDoubleClicked ? handleRowDoubleClicked : undefined
          }
          suppressDragLeaveHidesColumns
        />
      </div>
      <span role="status" className="sr-only">
        {copyAnnouncement(status)}
      </span>
    </section>
  );
}
