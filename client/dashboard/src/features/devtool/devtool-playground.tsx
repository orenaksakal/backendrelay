import { Loader2, Play } from 'lucide-react';
import { useEffect, useMemo, useRef, useState } from 'react';
import { CodeEditor } from '@/components/code-editor/code-editor';
import { JsonTree } from '@/components/data/json-tree';
import {
  SimpleTable,
  type SimpleTableColumn,
} from '@/components/data/simple-table';
import { Button } from '@/components/ui/button';
import { Switch } from '@/components/ui/switch';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { AppDataGate } from '@/components/data/app-data-gate';
import type { App, EffectiveRole } from '@/lib/api/endpoints/schemas';
import { peekAppClient, useAppSchema } from '@/lib/instant';
import {
  defaultInstaqlQuery,
  parseInstaqlQuery,
  playgroundCell,
  playgroundStatus,
  playgroundTable,
  queryResultCount,
  queryResultError,
  queryResultHasData,
} from './model';

export interface DevtoolPlaygroundProps {
  app: App;
  role: EffectiveRole;
}

type ResultView = 'tree' | 'table' | 'json';

interface RunRequest {
  query: Record<string, unknown>;
}

interface RunResult {
  result: unknown;
  ms: number;
}

/**
 * C.20 Playground tab: the query half of C.12. It uses the shared app client
 * (admin token), can follow updates with the Live switch, and shows the first
 * namespace as a table or the whole result as a tree or JSON.
 */
export function DevtoolPlayground({ app, role }: DevtoolPlaygroundProps) {
  return (
    <AppDataGate app={app} role={role} thing="playground">
      <DevtoolPlaygroundData appId={app.id} />
    </AppDataGate>
  );
}

function DevtoolPlaygroundData({ appId }: { appId: string }) {
  const client = peekAppClient(appId);
  const schema = useAppSchema(appId);
  const [text, setText] = useState('');
  const [live, setLive] = useState(false);
  const [request, setRequest] = useState<RunRequest | null>(null);
  const [state, setState] = useState<RunResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [running, setRunning] = useState(false);
  const [view, setView] = useState<ResultView>('tree');
  const edited = useRef(false);

  // The editor opens on the first namespace once the schema arrives, and never
  // replaces a query the user has already typed.
  useEffect(() => {
    if (edited.current || schema.isLoading) return;
    setText(defaultInstaqlQuery(schema.namespaces));
  }, [schema.isLoading, schema.namespaces]);

  useEffect(() => {
    if (client === undefined || request === null) return;
    let unsubscribe: (() => void) | undefined;
    const runState = { stopped: false };
    const started = performance.now();

    function stop(): void {
      runState.stopped = true;
      unsubscribe?.();
      unsubscribe = undefined;
    }

    // eslint-disable-next-line react-hooks/set-state-in-effect -- a new subscription starts in the running state
    setRunning(true);
    setError(null);

    unsubscribe = client.core.subscribeQuery(request.query, (result) => {
      const failure = queryResultError(result);
      if (failure !== null) {
        setState(null);
        setError(failure);
        setRunning(false);
        stop();
        return;
      }
      // The subscription can announce loading before the first payload.
      if (!queryResultHasData(result)) return;
      setState({
        result,
        ms: Math.max(0, Math.round(performance.now() - started)),
      });
      setError(null);
      setRunning(false);
      // A one-shot run keeps the first answer and stops the subscription.
      if (!live) stop();
    });

    // A synchronous subscription (tests, cached data) settles before the
    // unsubscribe handle exists.
    if (runState.stopped) stop();

    return () => {
      unsubscribe?.();
    };
  }, [client, request, live]);

  const table = useMemo(
    () => (state === null ? null : playgroundTable(state.result)),
    [state],
  );

  function run(): void {
    const parsed = parseInstaqlQuery(text);
    if (!parsed.ok) {
      setError(parsed.message);
      setState(null);
      return;
    }
    setError(null);
    setState(null);
    setRequest({ query: parsed.query });
  }

  const tableColumns = useMemo(() => {
    if (table === null) return [];
    return table.columns.map<SimpleTableColumn<Record<string, unknown>>>(
      (column) => ({
        id: column,
        header: column,
        cell: (row) => (
          <span className="font-mono break-all">
            {playgroundCell(row[column])}
          </span>
        ),
      }),
    );
  }, [table]);

  return (
    <div className="flex h-full min-h-0 flex-col gap-2">
      {/* eslint-disable-next-line jsx-a11y/no-static-element-interactions -- keyboard shortcut wrapper around the editor */}
      <div
        className="flex flex-col gap-2"
        onKeyDown={(event) => {
          if (event.key !== 'Enter') return;
          if (!event.metaKey && !event.ctrlKey) return;
          event.preventDefault();
          run();
        }}
      >
        <CodeEditor
          value={text}
          mode="instaql"
          aria-label="InstaQL query"
          height="150px"
          onChange={(next) => {
            edited.current = true;
            setText(next);
          }}
        />
        <div className="flex items-center gap-2">
          <Button
            type="button"
            size="sm"
            onClick={run}
            aria-busy={running}
            aria-keyshortcuts="Control+Enter Meta+Enter"
          >
            {running ? (
              <Loader2 className="animate-spin" aria-hidden="true" />
            ) : (
              <Play aria-hidden="true" />
            )}
            Run
          </Button>
          <Switch
            aria-label="Live updates"
            checked={live}
            onCheckedChange={setLive}
          />
          <span className="text-muted-foreground text-xs" aria-hidden="true">
            Live
          </span>
        </div>
      </div>

      {error === null ? null : (
        <p role="alert" className="text-destructive text-xs">
          {error}
        </p>
      )}

      {state === null ? (
        error === null ? (
          <p className="text-muted-foreground text-xs">
            {`Run a query to see ${schema.isLoading ? 'data' : 'results'}.`}
          </p>
        ) : null
      ) : (
        <>
          <p role="status" className="text-muted-foreground text-xs">
            {playgroundStatus(queryResultCount(state.result), state.ms)}
          </p>
          <Tabs
            value={view}
            onValueChange={(value) => {
              if (value === 'tree' || value === 'table' || value === 'json') {
                setView(value as ResultView);
              }
            }}
            className="flex min-h-0 flex-1 flex-col"
          >
            <TabsList aria-label="Result views">
              <TabsTrigger value="tree">Tree</TabsTrigger>
              <TabsTrigger value="table">Table</TabsTrigger>
              <TabsTrigger value="json">JSON</TabsTrigger>
            </TabsList>
            <TabsContent value="tree" className="min-h-0 flex-1 overflow-auto">
              <JsonTree value={state.result} aria-label="Query result" />
            </TabsContent>
            <TabsContent value="table" className="min-h-0 flex-1 overflow-auto">
              {table === null ? (
                <p className="text-muted-foreground text-xs">
                  No entities in this result.
                </p>
              ) : (
                <SimpleTable
                  caption={`${table.namespace} results`}
                  columns={tableColumns}
                  rows={table.rows}
                  getRowId={(row) =>
                    typeof row['id'] === 'string'
                      ? row['id']
                      : JSON.stringify(row)
                  }
                />
              )}
            </TabsContent>
            <TabsContent value="json" className="min-h-0 flex-1 overflow-auto">
              <pre className="text-cell font-mono break-all whitespace-pre-wrap">
                {JSON.stringify(state.result, null, 2)}
              </pre>
            </TabsContent>
          </Tabs>
        </>
      )}
    </div>
  );
}
