import { Play, Table2 } from 'lucide-react';
import { CodeBlock } from '@/components/code-editor/code-block';
import { JsonTree } from '@/components/data/json-tree';
import {
  SimpleTable,
  type SimpleTableColumn,
} from '@/components/data/simple-table';
import { EmptyState } from '@/components/feedback/empty-state';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import type { PermissionsCheckRow } from '@/lib/permissions/check';
import { formatCount } from '@/lib/format';
import {
  countEntities,
  firstNamespaceRows,
  queryStatus,
} from '@/lib/playground/model';
import { cn } from '@/lib/utils';
import { ChecksTable } from './checks-table';

/**
 * C.12 results: Tree, Table (first namespace), JSON and Checks for a query;
 * the permission checks and the full server response for a transaction.
 */

export type QueryResultTab = 'tree' | 'table' | 'json' | 'checks';
export type TransactResultTab = 'checks' | 'response';

export interface QueryRunResult {
  kind: 'query';
  result: unknown;
  checks: PermissionsCheckRow[];
  durationMs: number;
  live: boolean;
}

export interface TransactRunResult {
  kind: 'transact';
  response: unknown;
  checks: PermissionsCheckRow[];
  durationMs: number;
  allChecksOk: boolean;
  committed: boolean;
}

export type PlaygroundResult = QueryRunResult | TransactRunResult;

interface ResultRow {
  key: string;
  values: Record<string, unknown>;
}

function cellText(value: unknown): string {
  if (value === null || value === undefined) return '—';
  if (typeof value === 'string') return value;
  if (typeof value === 'number' || typeof value === 'boolean') {
    return String(value);
  }
  return JSON.stringify(value);
}

function ResultTable({ result }: { result: unknown }) {
  const data = firstNamespaceRows(result);
  if (data === null) {
    return (
      <EmptyState
        icon={Table2}
        title="Nothing to tabulate"
        description="This result has no namespace rows. Try the JSON tab."
      />
    );
  }

  const rows: ResultRow[] = data.rows.map((row, index) => ({
    key:
      typeof row['id'] === 'string' && row['id'] !== ''
        ? row['id']
        : `row-${String(index)}`,
    values: row,
  }));
  const columns: ReadonlyArray<SimpleTableColumn<ResultRow>> = data.columns.map(
    (column) => ({
      id: column,
      header: column,
      cell: (row) => (
        <span
          className="text-cell block max-w-96 truncate font-mono"
          title={cellText(row.values[column])}
        >
          {cellText(row.values[column])}
        </span>
      ),
    }),
  );

  return (
    <SimpleTable
      caption={`${data.namespace} results`}
      columns={columns}
      rows={rows}
      getRowId={(row) => row.key}
      empty="No rows returned."
    />
  );
}

export function QueryResults({
  result,
  tab,
  onTabChange,
  actorIsAdmin,
}: {
  result: QueryRunResult | null;
  tab: QueryResultTab;
  onTabChange: (tab: QueryResultTab) => void;
  actorIsAdmin: boolean;
}) {
  if (result === null) {
    return (
      <EmptyState
        icon={Play}
        title="No query result yet"
        description="Write an InstaQL query on the left, then press Run."
      />
    );
  }

  const count = countEntities(result.result);

  return (
    <div className="flex min-h-0 flex-col gap-2">
      <p role="status" className="text-muted-foreground text-xs">
        {queryStatus(count, result.durationMs)}
        {result.live ? ' · live' : ''}
      </p>
      <Tabs
        value={tab}
        onValueChange={(value) => {
          if (
            value === 'tree' ||
            value === 'table' ||
            value === 'json' ||
            value === 'checks'
          ) {
            onTabChange(value as QueryResultTab);
          }
        }}
      >
        <TabsList aria-label="Query results">
          <TabsTrigger value="tree">Tree</TabsTrigger>
          <TabsTrigger value="table">Table</TabsTrigger>
          <TabsTrigger value="json">JSON</TabsTrigger>
          {actorIsAdmin ? null : (
            <TabsTrigger value="checks">Checks</TabsTrigger>
          )}
        </TabsList>
        <TabsContent value="tree">
          <JsonTree
            aria-label="Query result"
            value={result.result}
            className="max-h-[52vh]"
          />
        </TabsContent>
        <TabsContent value="table">
          <div className="max-h-[52vh] overflow-auto">
            <ResultTable result={result.result} />
          </div>
        </TabsContent>
        <TabsContent value="json">
          <CodeBlock
            code={JSON.stringify(result.result, null, 2)}
            language="json"
            className="max-h-[52vh] overflow-auto"
          />
        </TabsContent>
        {actorIsAdmin ? null : (
          <TabsContent value="checks">
            {result.checks.length === 0 ? (
              <p className="text-muted-foreground text-sm">
                The server returned no permission checks for this query.
              </p>
            ) : (
              <ChecksTable checks={result.checks} />
            )}
          </TabsContent>
        )}
      </Tabs>
    </div>
  );
}

export function TransactResults({
  result,
  tab,
  onTabChange,
}: {
  result: TransactRunResult | null;
  tab: TransactResultTab;
  onTabChange: (tab: TransactResultTab) => void;
}) {
  if (result === null) {
    return (
      <EmptyState
        icon={Play}
        title="No transaction result yet"
        description="Add JSON steps on the left, then press Run."
      />
    );
  }

  return (
    <div className="flex min-h-0 flex-col gap-2">
      <p
        role="status"
        className={cn(
          'text-xs',
          result.committed ? 'text-success' : 'text-muted-foreground',
        )}
      >
        {result.committed ? 'Committed' : 'Not committed'} ·{' '}
        {formatCount(result.checks.length)}{' '}
        {result.checks.length === 1 ? 'check' : 'checks'} ·{' '}
        {String(Math.round(result.durationMs))} ms
      </p>
      <p
        className={cn(
          'text-sm',
          result.allChecksOk ? 'text-success' : 'text-destructive',
        )}
      >
        {result.allChecksOk
          ? 'Every permission check passed.'
          : 'Some permission checks did not pass.'}
      </p>
      <Tabs
        value={tab}
        onValueChange={(value) => {
          if (value === 'checks' || value === 'response')
            onTabChange(value as TransactResultTab);
        }}
      >
        <TabsList aria-label="Transaction results">
          <TabsTrigger value="checks">Checks</TabsTrigger>
          <TabsTrigger value="response">Response</TabsTrigger>
        </TabsList>
        <TabsContent value="checks">
          {result.checks.length === 0 ? (
            <p className="text-muted-foreground text-sm">
              The server returned no permission checks for these steps.
            </p>
          ) : (
            <ChecksTable checks={result.checks} />
          )}
        </TabsContent>
        <TabsContent value="response">
          <CodeBlock
            code={JSON.stringify(result.response, null, 2)}
            language="json"
            className="max-h-[52vh] overflow-auto"
          />
        </TabsContent>
      </Tabs>
    </div>
  );
}
