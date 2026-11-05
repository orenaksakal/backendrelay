import type { DataGridProps } from '@/components/data-grid/data-grid';

/**
 * jsdom stand-in for AG Grid in Explorer unit tests (the plan tests the real
 * grid in Playwright). It exposes the row count, ids and headers so the page
 * test can check what reached the grid without a canvas grid.
 */
export default function DataGridStub<Row>({
  rowData,
  columnDefs,
}: DataGridProps<Row>) {
  return (
    <div data-testid="data-grid">
      <p>{`${String(rowData.length)} rows`}</p>
      <ul>
        {rowData.map((row, index) => {
          const value: unknown =
            typeof row === 'object' && row !== null
              ? (row as { id?: unknown }).id
              : index;
          return <li key={index}>{String(value)}</li>;
        })}
      </ul>
      <ul>
        {columnDefs.map((column) => (
          <li key={String(column.colId)}>{String(column.headerName)}</li>
        ))}
      </ul>
    </div>
  );
}
