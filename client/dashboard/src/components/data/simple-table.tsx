import { ArrowDown, ArrowUp } from 'lucide-react';
import type * as React from 'react';
import { TableSkeleton } from '@/components/feedback/skeletons';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
import {
  Table,
  TableBody,
  TableCaption,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { formatCount } from '@/lib/format';
import { cn } from '@/lib/utils';

export interface SimpleTableColumn<Row> {
  id: string;
  header: React.ReactNode;
  sortable?: boolean;
  align?: 'left' | 'right';
  className?: string;
  cell: (row: Row) => React.ReactNode;
}

export interface SimpleTableSort {
  id: string;
  dir: 'asc' | 'desc';
}

export interface SimpleTableProps<Row> {
  /** Accessible name of the table. */
  caption: string;
  columns: ReadonlyArray<SimpleTableColumn<Row>>;
  rows: ReadonlyArray<Row>;
  getRowId: (row: Row) => string;
  sort?: SimpleTableSort | null;
  onSortChange?: (sort: SimpleTableSort) => void;
  selectable?: boolean;
  selectedIds?: ReadonlyArray<string>;
  onSelectionChange?: (ids: string[]) => void;
  /**
   * Mouse shortcut for opening a row. Keyboard users activate the link or
   * button in the row's first cell instead; clicks inside interactive
   * elements are ignored here.
   */
  onRowActivate?: (row: Row) => void;
  loading?: boolean;
  /** Shown in a full-width row when there are no rows. */
  empty?: React.ReactNode;
  /** Extra actions next to "{n} selected". */
  bulkActions?: (selectedRows: ReadonlyArray<Row>) => React.ReactNode;
  className?: string;
}

function sortDirection(
  column: { id: string; sortable?: boolean },
  sort: SimpleTableSort | null | undefined,
): 'ascending' | 'descending' | 'none' | undefined {
  if (!column.sortable) {
    return undefined;
  }
  if (sort?.id !== column.id) {
    return 'none';
  }
  return sort.dir === 'asc' ? 'ascending' : 'descending';
}

/** Interactive elements keep their own click; the row does not open under them. */
const INTERACTIVE_SELECTOR =
  'a, button, input, select, textarea, [role="checkbox"], [role="menuitem"]';

/**
 * Native table with sticky header, sortable header buttons, an optional
 * selection column with a floating bulk bar, and 40px rows (B.4).
 */
export function SimpleTable<Row>({
  caption,
  columns,
  rows,
  getRowId,
  sort,
  onSortChange,
  selectable = false,
  selectedIds = [],
  onSelectionChange,
  onRowActivate,
  loading = false,
  empty,
  bulkActions,
  className,
}: SimpleTableProps<Row>) {
  if (loading) {
    return <TableSkeleton columns={columns.length + 1} className={className} />;
  }

  const selected = new Set(selectedIds);
  const selectedRows = rows.filter((row) => selected.has(getRowId(row)));
  const allSelected = rows.length > 0 && selectedRows.length === rows.length;
  const someSelected = selectedRows.length > 0 && !allSelected;
  const columnCount = columns.length + (selectable ? 1 : 0);

  function toggleAll(checked: boolean) {
    onSelectionChange?.(checked ? rows.map((row) => getRowId(row)) : []);
  }

  function toggleRow(id: string, checked: boolean) {
    if (!onSelectionChange) {
      return;
    }
    const next = new Set(selected);
    if (checked) {
      next.add(id);
    } else {
      next.delete(id);
    }
    onSelectionChange([...next]);
  }

  function toggleSort(column: SimpleTableColumn<Row>) {
    if (!column.sortable || !onSortChange) {
      return;
    }
    const current = sort?.id === column.id ? sort.dir : undefined;
    onSortChange({ id: column.id, dir: current === 'asc' ? 'desc' : 'asc' });
  }

  return (
    <div className={cn('flex flex-col gap-2', className)}>
      <Table>
        <TableCaption className="sr-only">{caption}</TableCaption>
        <TableHeader>
          <TableRow>
            {selectable ? (
              <TableHead className="w-10">
                <span className="sr-only">Select</span>
                <Checkbox
                  aria-label="Select all rows"
                  checked={allSelected}
                  indeterminate={someSelected}
                  onCheckedChange={(checked) => {
                    toggleAll(checked);
                  }}
                />
              </TableHead>
            ) : null}
            {columns.map((column) => {
              const direction = sortDirection(column, sort);
              return (
                <TableHead
                  key={column.id}
                  aria-sort={direction}
                  className={cn(
                    column.align === 'right' && 'text-right',
                    column.className,
                  )}
                >
                  {column.sortable && onSortChange ? (
                    <button
                      type="button"
                      className="hover:text-foreground inline-flex min-h-6 items-center gap-1"
                      onClick={() => {
                        toggleSort(column);
                      }}
                    >
                      {column.header}
                      {direction === 'ascending' ? (
                        <ArrowUp className="size-3.5" aria-hidden="true" />
                      ) : direction === 'descending' ? (
                        <ArrowDown className="size-3.5" aria-hidden="true" />
                      ) : null}
                    </button>
                  ) : (
                    column.header
                  )}
                </TableHead>
              );
            })}
          </TableRow>
        </TableHeader>
        <TableBody>
          {rows.length === 0 ? (
            <TableRow>
              <TableCell colSpan={columnCount} className="text-center">
                {empty ?? 'No rows'}
              </TableCell>
            </TableRow>
          ) : (
            rows.map((row) => {
              const id = getRowId(row);
              const isSelected = selected.has(id);
              const rowInteraction = onRowActivate
                ? {
                    onClick: (event: React.MouseEvent<HTMLTableRowElement>) => {
                      const target = event.target;
                      if (
                        target instanceof Element &&
                        target.closest(INTERACTIVE_SELECTOR)
                      ) {
                        return;
                      }
                      onRowActivate(row);
                    },
                  }
                : {};

              return (
                <TableRow
                  key={id}
                  data-state={isSelected ? 'selected' : undefined}
                  className={cn(onRowActivate && 'cursor-pointer')}
                  {...rowInteraction}
                >
                  {selectable ? (
                    <TableCell className="w-10">
                      <Checkbox
                        aria-label={`Select row ${id}`}
                        checked={isSelected}
                        onCheckedChange={(checked) => {
                          toggleRow(id, checked);
                        }}
                      />
                    </TableCell>
                  ) : null}
                  {columns.map((column) => (
                    <TableCell
                      key={column.id}
                      className={cn(
                        column.align === 'right' && 'text-right tabular-nums',
                        column.className,
                      )}
                    >
                      {column.cell(row)}
                    </TableCell>
                  ))}
                </TableRow>
              );
            })
          )}
        </TableBody>
      </Table>

      {selectable && selectedRows.length > 0 ? (
        <div className="border-border bg-card sticky bottom-2 z-(--z-sticky) mx-auto flex w-fit flex-wrap items-center gap-3 rounded-lg border px-3 py-1.5 shadow-md">
          <span role="status" className="text-xs">
            {formatCount(selectedRows.length)} selected
          </span>
          {bulkActions?.(selectedRows)}
          <Button
            type="button"
            variant="ghost"
            size="sm"
            onClick={() => {
              onSelectionChange?.([]);
            }}
          >
            Clear selection
          </Button>
        </div>
      ) : null}
    </div>
  );
}
