import { useQuery } from '@tanstack/react-query';
import { Play, TriangleAlert } from 'lucide-react';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { AppDataGate } from '@/components/data/app-data-gate';
import { CodeEditor } from '@/components/code-editor/code-editor';
import { PageHeader } from '@/components/layout/page-header';
import { Button } from '@/components/ui/button';
import {
  Combobox,
  ComboboxContent,
  ComboboxInput,
  ComboboxInputGroup,
  ComboboxItem,
  ComboboxList,
  ComboboxTrigger,
} from '@/components/ui/combobox';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { Kbd } from '@/components/ui/kbd';
import { Label } from '@/components/ui/label';
import {
  ResizableHandle,
  ResizablePanel,
  ResizablePanelGroup,
} from '@/components/ui/resizable';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { Switch } from '@/components/ui/switch';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import type { App, EffectiveRole } from '@/lib/api/endpoints/schemas';
import { peekAppClient, useAppSchema } from '@/lib/instant';
import {
  runDebugQuery,
  runDebugTransact,
  searchTestAsUsers,
} from '@/lib/permissions/check';
import { parseRules, type Rules } from '@/lib/permissions/model';
import {
  newRunId,
  readPlaygroundHistory,
  recordPlaygroundRun,
  savePlaygroundRunName,
  writePlaygroundHistory,
  type PlaygroundRun,
} from '@/lib/playground/history';
import { instaqlCompletionSource } from '@/lib/playground/instaql';
import {
  actorLabel,
  defaultPlaygroundQuery,
  lintQueryDocument,
  lintTransactionSteps,
  liveQueryError,
  parseQueryDocument,
  parseTransactionSteps,
  playgroundActor,
} from '@/lib/playground/model';
import {
  isPlaygroundActor,
  isPlaygroundMode,
  PLAYGROUND_ACTORS,
  PLAYGROUND_ACTOR_LABELS,
  playgroundSearchFor,
  type PlaygroundActorKind,
  type PlaygroundMode,
  type PlaygroundSearch,
} from '@/lib/playground/url-state';
import { getRuntimeConfig } from '@/lib/runtime-config';
import { formatShortcut } from '@/lib/shortcuts';
import { toastManager } from '@/lib/toast';
import { appAdminConfig } from '@/lib/users/admin';
import { useMediaQuery } from '@/lib/use-media-query';
import { PlaygroundHistoryMenu } from './history-menu';
import {
  QueryResults,
  TransactResults,
  type PlaygroundResult,
  type QueryResultTab,
  type TransactResultTab,
} from './query-results';
import { PlaygroundRulesSheet } from './rules-sheet';

export interface PlaygroundPageProps {
  app: App;
  role: EffectiveRole;
  search: PlaygroundSearch;
  onSearchChange: (search: PlaygroundSearch) => void;
}

/** C.12 Playground: query and transact against the app with its debug APIs. */
export function PlaygroundPage({
  app,
  role,
  search,
  onSearchChange,
}: PlaygroundPageProps) {
  return (
    <div className="flex flex-col gap-4">
      <PageHeader
        title="Playground"
        description="Run InstaQL queries and admin transactions against this app."
      />
      <AppDataGate
        app={app}
        role={role}
        thing="playground"
        forbiddenMessage="The playground needs admin access to this app."
      >
        <PlaygroundWorkspace
          key={app.id}
          app={app}
          search={search}
          onSearchChange={onSearchChange}
        />
      </AppDataGate>
    </div>
  );
}

interface PlaygroundWorkspaceProps {
  app: App;
  search: PlaygroundSearch;
  onSearchChange: PlaygroundPageProps['onSearchChange'];
}

function errorMessage(error: unknown): string {
  return error instanceof Error && error.message.trim() !== ''
    ? error.message
    : 'The request failed. Check the connection and try again.';
}

function PlaygroundWorkspace({
  app,
  search,
  onSearchChange,
}: PlaygroundWorkspaceProps) {
  const client = peekAppClient(app.id) ?? null;
  const schema = useAppSchema(app.id);
  const wide = useMediaQuery('(min-width: 1024px)');

  const config = useMemo(
    () =>
      appAdminConfig({
        appId: app.id,
        adminToken: app.admin_token,
        apiBaseUrl: getRuntimeConfig()?.apiBaseUrl,
      }),
    [app.id, app.admin_token],
  );

  const mode = search.mode;
  const actorKind = search.as;

  const [queryText, setQueryText] = useState('');
  const [stepsText, setStepsText] = useState('[]');
  const [email, setEmail] = useState('');
  const [token, setToken] = useState('');
  const [userSearch, setUserSearch] = useState('');
  const [debouncedSearch, setDebouncedSearch] = useState('');
  const [useCustomRules, setUseCustomRules] = useState(false);
  const [rulesText, setRulesText] = useState(() =>
    JSON.stringify(app.rules ?? {}, null, 2),
  );
  const [rulesOpen, setRulesOpen] = useState(false);
  const [live, setLive] = useState(true);
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<PlaygroundResult | null>(null);
  const [queryTab, setQueryTab] = useState<QueryResultTab>('tree');
  const [transactTab, setTransactTab] = useState<TransactResultTab>('checks');
  const [runs, setRuns] = useState<PlaygroundRun[]>(() =>
    readPlaygroundHistory(app.id),
  );
  const [lastRunId, setLastRunId] = useState<string | null>(null);
  const [saveOpen, setSaveOpen] = useState(false);
  const [saveName, setSaveName] = useState('');

  const liveCleanup = useRef<(() => void) | null>(null);
  const defaultQuerySet = useRef(false);

  const namespaces = schema.namespaces;
  const namespaceNames = useMemo(
    () => namespaces.map((namespace) => namespace.name),
    [namespaces],
  );
  const completions = useMemo(
    () => instaqlCompletionSource(namespaces),
    [namespaces],
  );
  const exampleNamespace =
    namespaceNames.find((name) => !name.startsWith('$')) ?? 'todos';

  const stopLive = useCallback(() => {
    liveCleanup.current?.();
    liveCleanup.current = null;
  }, []);

  useEffect(() => {
    return () => {
      stopLive();
    };
  }, [stopLive]);

  useEffect(() => {
    if (!live) stopLive();
  }, [live, stopLive]);

  // The first schema load picks a starter query; later edits are never replaced.
  useEffect(() => {
    if (defaultQuerySet.current || schema.isLoading) return;
    defaultQuerySet.current = true;
    setQueryText((current) =>
      current.trim() === '' ? defaultPlaygroundQuery(namespaceNames) : current,
    );
  }, [namespaceNames, schema.isLoading]);

  useEffect(() => {
    const timer = setTimeout(() => {
      setDebouncedSearch(userSearch);
    }, 200);
    return () => {
      clearTimeout(timer);
    };
  }, [userSearch]);

  useEffect(() => {
    if (actorKind === 'admin' && queryTab === 'checks') {
      // eslint-disable-next-line react-hooks/set-state-in-effect -- admins cannot use the checks tab
      setQueryTab('tree');
    }
  }, [actorKind, queryTab]);

  const users = useQuery({
    queryKey: [
      'dash',
      'apps',
      app.id,
      'playground-users',
      debouncedSearch,
      config,
    ],
    queryFn: () => {
      if (config === null) {
        throw new Error('The app admin token is not available.');
      }
      return searchTestAsUsers(config, debouncedSearch);
    },
    enabled: actorKind === 'email' && config !== null,
    staleTime: 30_000,
  });

  function selectMode(next: PlaygroundMode): void {
    stopLive();
    setResult(null);
    setError(null);
    onSearchChange(playgroundSearchFor({ mode: next, actor: actorKind }));
  }

  function selectActor(next: PlaygroundActorKind): void {
    stopLive();
    setResult(null);
    setError(null);
    onSearchChange(playgroundSearchFor({ mode, actor: next }));
  }

  function record(source: string): void {
    const entry: PlaygroundRun = {
      id: newRunId(),
      mode,
      actor: actorKind,
      actorLabel: actorLabel(actorKind, email),
      email: actorKind === 'email' ? email.trim() : undefined,
      source,
      createdAt: new Date().toISOString(),
    };
    const recorded = recordPlaygroundRun(runs, entry);
    setRuns(recorded.runs);
    writePlaygroundHistory(app.id, recorded.runs);
    setLastRunId(recorded.entry.id);
  }

  async function run(): Promise<void> {
    if (running) return;
    if (config === null) {
      setError(
        "The admin token for this app isn't available. Reload the app to fetch it again.",
      );
      return;
    }
    const actor = playgroundActor(actorKind, email, token);
    if (typeof actor === 'string') {
      setError(actor);
      return;
    }

    let rules: Rules | undefined;
    if (useCustomRules) {
      try {
        rules = parseRules(rulesText);
      } catch (failure) {
        setError(errorMessage(failure));
        return;
      }
    }

    setError(null);
    setResult(null);

    if (mode === 'transact') {
      let steps: unknown[];
      try {
        steps = parseTransactionSteps(stepsText);
      } catch (failure) {
        setError(errorMessage(failure));
        return;
      }
      record(stepsText);
      setRunning(true);
      try {
        const started = performance.now();
        const response = await runDebugTransact(config, {
          steps,
          rules,
          actor,
          commit: true,
        });
        setResult({
          kind: 'transact',
          response: response.response,
          checks: response.checks,
          allChecksOk: response.allChecksOk,
          committed: response.committed,
          durationMs: performance.now() - started,
        });
      } catch (failure) {
        setError(errorMessage(failure));
      } finally {
        setRunning(false);
      }
      return;
    }

    let query: Record<string, unknown>;
    try {
      query = parseQueryDocument(queryText);
    } catch (failure) {
      setError(errorMessage(failure));
      return;
    }
    record(queryText);

    // Admin runs can stay live through the shared client; every other actor is
    // a one-shot debug request with its impersonation header (C.12). Custom
    // rules only exist on the debug endpoints, so they always run one-shot.
    if (actor.kind === 'admin' && live && !useCustomRules && client !== null) {
      stopLive();
      setRunning(true);
      const started = performance.now();
      try {
        const unsubscribe = client.core.subscribeQuery(query, (value) => {
          const problem = liveQueryError(value);
          if (problem !== null) {
            stopLive();
            setError(problem);
            setRunning(false);
            return;
          }
          setResult({
            kind: 'query',
            result: value,
            checks: [],
            durationMs: performance.now() - started,
            live: true,
          });
          setRunning(false);
        });
        liveCleanup.current = unsubscribe;
      } catch (failure) {
        setError(errorMessage(failure));
        setRunning(false);
      }
      return;
    }

    stopLive();
    setRunning(true);
    try {
      const started = performance.now();
      const response = await runDebugQuery(config, { query, rules, actor });
      setResult({
        kind: 'query',
        result: response.result,
        checks: response.checks,
        durationMs: performance.now() - started,
        live: false,
      });
    } catch (failure) {
      setError(errorMessage(failure));
    } finally {
      setRunning(false);
    }
  }

  const runRef = useRef(run);
  useEffect(() => {
    runRef.current = run;
  });

  // ⌘↵ runs from anywhere on the page, including inside the CodeMirror
  // editors, where the global shortcut handler stays quiet (B.7). Dialogs and
  // sheets keep their own Enter behaviour.
  useEffect(() => {
    const listener = (event: KeyboardEvent) => {
      if ((!event.metaKey && !event.ctrlKey) || event.key !== 'Enter') return;
      const target = event.target;
      if (
        target instanceof Element &&
        target.closest('[role="dialog"]') !== null
      ) {
        return;
      }
      event.preventDefault();
      void runRef.current();
    };
    window.addEventListener('keydown', listener);
    return () => {
      window.removeEventListener('keydown', listener);
    };
  }, []);

  function restoreRun(entry: PlaygroundRun): void {
    stopLive();
    setResult(null);
    setError(null);
    if (entry.mode === 'query') {
      setQueryText(entry.source);
    } else {
      setStepsText(entry.source);
    }
    setEmail(entry.actor === 'email' ? (entry.email ?? '') : '');
    setToken('');
    setLastRunId(entry.id);
    onSearchChange(
      playgroundSearchFor({ mode: entry.mode, actor: entry.actor }),
    );
  }

  function openSaveAs(): void {
    const target =
      runs.find((entry) => entry.id === lastRunId) ?? runs[0] ?? null;
    if (target === null) return;
    setSaveName(target.name ?? '');
    setSaveOpen(true);
  }

  function submitSaveAs(): void {
    const target =
      runs.find((entry) => entry.id === lastRunId) ?? runs[0] ?? null;
    if (target === null || saveName.trim() === '') return;
    const next = savePlaygroundRunName(runs, target.id, saveName);
    setRuns(next);
    writePlaygroundHistory(app.id, next);
    setSaveOpen(false);
    toastManager.add({ type: 'success', title: 'Run saved' });
  }

  const editorPane = (
    <section
      aria-label={mode === 'query' ? 'Query editor' : 'Transaction editor'}
      className="border-border bg-card flex h-full min-h-[320px] flex-col gap-2 rounded-lg border p-3"
    >
      <div className="flex items-baseline justify-between gap-2">
        <h2 className="text-section">
          {mode === 'query' ? 'InstaQL query' : 'Transaction steps'}
        </h2>
        <span className="text-muted-foreground text-xs">
          {mode === 'query' ? 'instaql' : 'admin steps'}
        </span>
      </div>

      {mode === 'transact' ? (
        <div className="border-warning/40 bg-warning/5 flex items-start gap-2 rounded-md border p-2.5 text-sm">
          <TriangleAlert
            aria-hidden="true"
            className="text-warning mt-0.5 size-4 shrink-0"
          />
          <p>
            <span className="font-medium">
              Transactions change real data in {app.title}.
            </span>{' '}
            <span className="text-muted-foreground">
              Every step commits when the permission rules allow it.
            </span>
          </p>
        </div>
      ) : null}

      {mode === 'query' ? (
        <CodeEditor
          value={queryText}
          mode="instaql"
          aria-label="InstaQL query"
          height="340px"
          completions={completions}
          validate={lintQueryDocument}
          onChange={setQueryText}
        />
      ) : (
        <CodeEditor
          value={stepsText}
          mode="json"
          aria-label="Transaction steps"
          height="340px"
          validate={lintTransactionSteps}
          onChange={setStepsText}
        />
      )}

      <p className="text-muted-foreground text-xs">
        {mode === 'query' ? (
          <>
            Namespaces and attributes complete as you type. Press{' '}
            <Kbd>{formatShortcut('mod+enter')}</Kbd> to run.
          </>
        ) : (
          <>
            A JSON array of Admin SDK steps, for example{' '}
            <code className="font-mono">
              {JSON.stringify([
                [
                  'update',
                  exampleNamespace,
                  '<id>',
                  { title: 'Hello from the playground' },
                ],
              ])}
            </code>
            .
          </>
        )}
      </p>
    </section>
  );

  const resultsPane = (
    <section
      aria-label="Results"
      className="border-border bg-card flex h-full min-h-[320px] flex-col gap-2 rounded-lg border p-3"
    >
      <h2 className="text-section">Results</h2>
      {error !== null ? (
        <p
          role="alert"
          className="text-destructive text-sm whitespace-pre-line"
        >
          {error}
        </p>
      ) : mode === 'query' ? (
        <QueryResults
          result={result?.kind === 'query' ? result : null}
          tab={queryTab}
          onTabChange={setQueryTab}
          actorIsAdmin={actorKind === 'admin'}
        />
      ) : (
        <TransactResults
          result={result?.kind === 'transact' ? result : null}
          tab={transactTab}
          onTabChange={setTransactTab}
        />
      )}
    </section>
  );

  const panes = wide ? (
    <ResizablePanelGroup
      orientation="horizontal"
      className="min-h-[65vh] gap-2"
    >
      <ResizablePanel defaultSize="46%" minSize="28%" className="min-w-0">
        {editorPane}
      </ResizablePanel>
      <ResizableHandle />
      <ResizablePanel defaultSize="54%" minSize="28%" className="min-w-0 pl-2">
        {resultsPane}
      </ResizablePanel>
    </ResizablePanelGroup>
  ) : (
    <div className="flex flex-col gap-3">
      {editorPane}
      {resultsPane}
    </div>
  );

  return (
    <Tabs
      value={mode}
      onValueChange={(value) => {
        if (isPlaygroundMode(value)) selectMode(value);
      }}
      className="flex flex-col gap-3"
    >
      <div className="flex flex-col gap-2">
        <div className="flex flex-wrap items-center gap-2">
          <TabsList aria-label="Playground mode">
            <TabsTrigger value="query">Query</TabsTrigger>
            <TabsTrigger value="transact">Transact</TabsTrigger>
          </TabsList>
          <div className="ml-auto flex items-center gap-2">
            <PlaygroundHistoryMenu
              runs={runs}
              onRestore={restoreRun}
              onSaveAs={openSaveAs}
            />
            <Button
              type="button"
              aria-label={`${running ? 'Running…' : 'Run'} ${formatShortcut('mod+enter')}`}
              disabled={running}
              onClick={() => {
                void run();
              }}
            >
              <Play aria-hidden="true" />
              {running ? 'Running…' : 'Run'}{' '}
              <Kbd>{formatShortcut('mod+enter')}</Kbd>
            </Button>
          </div>
        </div>

        <div className="flex flex-wrap items-end gap-3">
          <div className="flex flex-col gap-1">
            <span className="text-muted-foreground text-xs">Run as</span>
            <Select
              value={actorKind}
              onValueChange={(value) => {
                if (isPlaygroundActor(value)) selectActor(value);
              }}
            >
              <SelectTrigger aria-label="Run as" className="w-40">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {PLAYGROUND_ACTORS.map((candidate) => (
                  <SelectItem key={candidate} value={candidate}>
                    {PLAYGROUND_ACTOR_LABELS[candidate]}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>

          {actorKind === 'email' ? (
            <div className="flex min-w-56 flex-1 flex-col gap-1">
              <span className="text-muted-foreground text-xs">User email</span>
              <Combobox
                value={email === '' ? null : email}
                onValueChange={(value) => {
                  setEmail(typeof value === 'string' ? value : '');
                }}
                onInputValueChange={(value) => {
                  setUserSearch(value);
                }}
              >
                <ComboboxInputGroup>
                  <ComboboxInput
                    aria-label="Search users by email"
                    placeholder="Search users by email"
                  />
                  <ComboboxTrigger aria-label="Show users" />
                </ComboboxInputGroup>
                <ComboboxContent>
                  <ComboboxList>
                    {users.data?.map((user) => (
                      <ComboboxItem key={user.id} value={user.email}>
                        {user.email}
                      </ComboboxItem>
                    ))}
                  </ComboboxList>
                </ComboboxContent>
              </Combobox>
            </div>
          ) : null}

          {actorKind === 'token' ? (
            <div className="flex min-w-56 flex-1 flex-col gap-1">
              <span className="text-muted-foreground text-xs">
                Refresh token
              </span>
              <Input
                type="password"
                aria-label="Refresh token"
                autoComplete="off"
                placeholder="Paste a refresh token"
                value={token}
                onChange={(event) => {
                  setToken(event.target.value);
                }}
              />
            </div>
          ) : null}

          <div className="ml-auto flex flex-wrap items-center gap-4 pb-1">
            <div className="flex items-center gap-2">
              <Switch
                aria-label="Use custom rules"
                checked={useCustomRules}
                onCheckedChange={(checked) => {
                  setUseCustomRules(checked);
                  if (checked) setRulesOpen(true);
                }}
              />
              <span className="text-sm">Use custom rules</span>
              {useCustomRules ? (
                <Button
                  type="button"
                  variant="link"
                  size="sm"
                  onClick={() => {
                    setRulesOpen(true);
                  }}
                >
                  Edit rules
                </Button>
              ) : null}
            </div>
            <div className="flex items-center gap-2">
              <Switch
                aria-label="Live updates (Admin runs)"
                checked={live && actorKind === 'admin' && !useCustomRules}
                disabled={actorKind !== 'admin' || useCustomRules}
                onCheckedChange={setLive}
              />
              <span className="text-sm">Live</span>
              {actorKind !== 'admin' ? (
                <span className="text-muted-foreground text-xs">
                  Admin only
                </span>
              ) : useCustomRules ? (
                <span className="text-muted-foreground text-xs">
                  One-shot with custom rules
                </span>
              ) : null}
            </div>
          </div>
        </div>
      </div>

      <TabsContent value="query">{mode === 'query' ? panes : null}</TabsContent>
      <TabsContent value="transact">
        {mode === 'transact' ? panes : null}
      </TabsContent>

      <PlaygroundRulesSheet
        open={rulesOpen}
        onOpenChange={setRulesOpen}
        value={rulesText}
        onChange={setRulesText}
      />

      <Dialog open={saveOpen} onOpenChange={setSaveOpen}>
        <DialogContent>
          <form
            onSubmit={(event) => {
              event.preventDefault();
              submitSaveAs();
            }}
          >
            <DialogHeader>
              <DialogTitle>Save this run</DialogTitle>
              <DialogDescription>
                Saved runs stay at the top of the History menu. Nothing is sent
                to the server.
              </DialogDescription>
            </DialogHeader>
            <div className="mt-3 flex flex-col gap-1.5">
              <Label htmlFor="playground-run-name">Name</Label>
              <Input
                id="playground-run-name"
                value={saveName}
                onChange={(event) => {
                  setSaveName(event.target.value);
                }}
              />
            </div>
            <DialogFooter>
              <Button
                type="button"
                variant="secondary"
                onClick={() => {
                  setSaveOpen(false);
                }}
              >
                Cancel
              </Button>
              <Button type="submit" disabled={saveName.trim() === ''}>
                Save run
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>
    </Tabs>
  );
}
