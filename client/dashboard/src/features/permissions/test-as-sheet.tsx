import { useQuery } from '@tanstack/react-query';
import { Play } from 'lucide-react';
import { useEffect, useMemo, useRef, useState } from 'react';
import { CodeEditor } from '@/components/code-editor/code-editor';
import { JsonTree } from '@/components/data/json-tree';
import {
  SimpleTable,
  type SimpleTableColumn,
} from '@/components/data/simple-table';
import { Badge } from '@/components/ui/badge';
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
import { Input } from '@/components/ui/input';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import {
  Sheet,
  SheetBody,
  SheetContent,
  SheetDescription,
  SheetFooter,
  SheetHeader,
  SheetTitle,
} from '@/components/ui/sheet';
import { Switch } from '@/components/ui/switch';
import type { App } from '@/lib/api/endpoints/schemas';
import {
  defaultTestQuery,
  runDebugQuery,
  runDebugTransact,
  searchTestAsUsers,
  type PermissionsCheckRow,
  type TestAsActor,
} from '@/lib/permissions/check';
import type { Rules } from '@/lib/permissions/model';
import { getRuntimeConfig } from '@/lib/runtime-config';
import { PAGE_ACTION_EVENT } from '@/lib/shortcuts';
import { appAdminConfig } from '@/lib/users/admin';
import { cn } from '@/lib/utils';

export interface PermissionsTestAsSheetProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  app: App;
  /** The shared editor draft, sent only while "Use draft rules" is on. */
  draftRules: Rules | null;
  /** Used for the starting query and to keep namespace names visible. */
  namespaces: readonly string[];
}

type ActorKind = 'guest' | 'user' | 'token';
type TestMode = 'query' | 'transaction';

type RunState =
  | {
      kind: 'query';
      result: unknown;
      checks: PermissionsCheckRow[];
    }
  | {
      kind: 'transaction';
      committed: boolean;
      allChecksOk: boolean;
      checks: PermissionsCheckRow[];
    };

/** `key` keeps duplicate checks apart in the table. */
type CheckRow = PermissionsCheckRow & { key: string };

function errorMessage(error: unknown): string {
  return error instanceof Error && error.message.trim() !== ''
    ? error.message
    : 'The request failed. Check the connection and try again.';
}

/**
 * C.10 Test-as: runs `debugQuery`/`debugTransact` as a guest, a user or a
 * pasted refresh token, shows the returned entities as a tree, and lists every
 * permission check the server evaluated.
 */
export function PermissionsTestAsSheet({
  open,
  onOpenChange,
  app,
  draftRules,
  namespaces,
}: PermissionsTestAsSheetProps) {
  const config = useMemo(
    () =>
      appAdminConfig({
        appId: app.id,
        adminToken: app.admin_token,
        apiBaseUrl: getRuntimeConfig()?.apiBaseUrl,
      }),
    [app.id, app.admin_token],
  );

  const [actorKind, setActorKind] = useState<ActorKind>('guest');
  const [email, setEmail] = useState('');
  const [token, setToken] = useState('');
  const [userSearch, setUserSearch] = useState('');
  const [debouncedSearch, setDebouncedSearch] = useState('');
  const [mode, setMode] = useState<TestMode>('query');
  const [queryText, setQueryText] = useState(() =>
    defaultTestQuery(namespaces),
  );
  const [stepsText, setStepsText] = useState('[]');
  const [useDraftRules, setUseDraftRules] = useState(true);
  const [commit, setCommit] = useState(false);
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<RunState | null>(null);

  useEffect(() => {
    const timer = setTimeout(() => {
      setDebouncedSearch(userSearch);
    }, 200);
    return () => {
      clearTimeout(timer);
    };
  }, [userSearch]);

  const users = useQuery({
    queryKey: [
      'dash',
      'apps',
      app.id,
      'test-as-users',
      debouncedSearch,
      config,
    ],
    queryFn: () => {
      if (config === null) {
        throw new Error('The app admin token is not available.');
      }
      return searchTestAsUsers(config, debouncedSearch);
    },
    enabled: open && actorKind === 'user' && config !== null,
    staleTime: 30_000,
  });

  function actor(): TestAsActor | string {
    if (actorKind === 'user') {
      if (email.trim() === '') return 'Choose a user, or enter their email.';
      return { kind: 'user', email };
    }
    if (actorKind === 'token') {
      if (token.trim() === '') {
        return 'Paste a refresh token, or run as a guest.';
      }
      return { kind: 'token', token };
    }
    return { kind: 'guest' };
  }

  function parseJson(textValue: string, label: string): unknown {
    try {
      return JSON.parse(textValue) as unknown;
    } catch {
      throw new Error(`${label} must be valid JSON.`);
    }
  }

  function selectMode(next: TestMode): void {
    setMode(next);
    setResult(null);
    setError(null);
  }

  async function run(): Promise<void> {
    if (config === null) {
      setError(
        "The admin token for this app isn't available. Reload the app to fetch it again.",
      );
      return;
    }
    const selectedActor = actor();
    if (typeof selectedActor === 'string') {
      setError(selectedActor);
      return;
    }

    let rules: Rules | undefined;
    if (useDraftRules) {
      if (draftRules === null) {
        setError('The draft is not valid JSON, so it cannot be tested.');
        return;
      }
      rules = draftRules;
    }

    setRunning(true);
    setError(null);
    setResult(null);
    try {
      if (mode === 'query') {
        const query = parseJson(queryText, 'The query');
        if (
          query === null ||
          typeof query !== 'object' ||
          Array.isArray(query)
        ) {
          throw new Error('The query must be a JSON object.');
        }
        const response = await runDebugQuery(config, {
          query: query as Record<string, unknown>,
          rules,
          actor: selectedActor,
        });
        setResult({
          kind: 'query',
          result: response.result,
          checks: response.checks,
        });
      } else {
        const steps = parseJson(stepsText, 'The transaction steps');
        if (!Array.isArray(steps)) {
          throw new Error('The transaction steps must be a JSON array.');
        }
        const response = await runDebugTransact(config, {
          steps: steps as unknown[],
          rules,
          actor: selectedActor,
          commit,
        });
        setResult({
          kind: 'transaction',
          committed: response.committed,
          allChecksOk: response.allChecksOk,
          checks: response.checks,
        });
      }
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

  // ⌘↵ runs the panel while it is the top layer (B.7).
  useEffect(() => {
    if (!open) return;
    const listener = (event: Event) => {
      const detail = (event as CustomEvent<{ action?: string } | undefined>)
        .detail;
      if (detail?.action !== 'run') return;
      event.preventDefault();
      void runRef.current();
    };
    window.addEventListener(PAGE_ACTION_EVENT, listener);
    return () => {
      window.removeEventListener(PAGE_ACTION_EVENT, listener);
    };
  }, [open]);

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

  return (
    <Sheet
      open={open}
      onOpenChange={(next) => {
        if (!next) {
          setToken('');
          setResult(null);
          setError(null);
        }
        onOpenChange(next);
      }}
    >
      <SheetContent size="md">
        <SheetHeader>
          <SheetTitle>Test as…</SheetTitle>
          <SheetDescription>
            Run a query or a transaction through the permission rules as someone
            else. Nothing is saved.
          </SheetDescription>
        </SheetHeader>

        <SheetBody className="flex flex-col gap-4">
          <div
            role="group"
            aria-label="Test mode"
            className="border-border flex w-fit items-center gap-1 rounded-md border p-1"
          >
            <Button
              type="button"
              size="sm"
              variant={mode === 'query' ? 'secondary' : 'ghost'}
              aria-pressed={mode === 'query'}
              onClick={() => {
                selectMode('query');
              }}
            >
              Query
            </Button>
            <Button
              type="button"
              size="sm"
              variant={mode === 'transaction' ? 'secondary' : 'ghost'}
              aria-pressed={mode === 'transaction'}
              onClick={() => {
                selectMode('transaction');
              }}
            >
              Transaction
            </Button>
          </div>

          <div className="flex flex-col gap-1.5">
            <span className="text-sm font-medium">Run as</span>
            <Select
              value={actorKind}
              onValueChange={(value) => {
                if (
                  value === 'guest' ||
                  value === 'user' ||
                  value === 'token'
                ) {
                  setActorKind(value);
                }
              }}
            >
              <SelectTrigger aria-label="Run as">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="guest">Guest</SelectItem>
                <SelectItem value="user">User</SelectItem>
                <SelectItem value="token">Token</SelectItem>
              </SelectContent>
            </Select>
          </div>

          {actorKind === 'user' ? (
            <div className="flex flex-col gap-1.5">
              <span className="text-sm font-medium">User email</span>
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
              <p className="text-muted-foreground text-xs">
                {users.isFetching
                  ? 'Searching…'
                  : users.data !== undefined && users.data.length === 0
                    ? 'No users match.'
                    : 'Matches are searched in $users by email.'}
              </p>
            </div>
          ) : null}

          {actorKind === 'token' ? (
            <div className="flex flex-col gap-1.5">
              <span className="text-sm font-medium">Refresh token</span>
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

          <div className="flex items-center justify-between gap-3">
            <div className="min-w-0">
              <p className="text-sm font-medium">Use draft rules</p>
              <p className="text-muted-foreground text-xs">
                Test the unsaved editor draft instead of the saved rules.
              </p>
            </div>
            <Switch
              aria-label="Use draft rules"
              checked={useDraftRules}
              onCheckedChange={setUseDraftRules}
            />
          </div>

          {mode === 'query' ? (
            <CodeEditor
              value={queryText}
              mode="instaql"
              aria-label="InstaQL query"
              height="160px"
              onChange={setQueryText}
            />
          ) : (
            <>
              <div
                className={cn(
                  'rounded-md border p-3 text-sm',
                  commit
                    ? 'border-warning/40 bg-warning/5'
                    : 'border-border bg-muted/40',
                )}
              >
                <p className="font-medium">
                  This runs against real data. Writes are rolled back only if a
                  rule denies them.
                </p>
                <p className="text-muted-foreground mt-1 text-xs">
                  {commit
                    ? 'Successful transactions are written to the app.'
                    : 'Dry run: nothing is written, whatever the rules decide.'}
                </p>
              </div>
              <div className="flex items-center justify-between gap-3">
                <div className="min-w-0">
                  <p className="text-sm font-medium">Write to DB</p>
                  <p className="text-muted-foreground text-xs">
                    Commits a transaction when every rule allows it.
                  </p>
                </div>
                <Switch
                  aria-label="Write to DB"
                  checked={commit}
                  onCheckedChange={setCommit}
                />
              </div>
              <CodeEditor
                value={stepsText}
                mode="json"
                aria-label="Transaction steps"
                height="160px"
                onChange={setStepsText}
              />
              <p className="text-muted-foreground text-xs">
                A JSON array of Admin SDK transaction tuples, for example{' '}
                <code className="font-mono">
                  {`["update", "todos", "<id>", {"title": "Done"}]`}
                </code>
                .
              </p>
            </>
          )}

          {error !== null ? (
            <p
              role="alert"
              className="text-destructive text-sm whitespace-pre-line"
            >
              {error}
            </p>
          ) : null}

          {result !== null ? (
            <div className="flex flex-col gap-3">
              <p role="status" className="text-muted-foreground text-sm">
                {result.checks.length === 1
                  ? '1 permission check'
                  : `${String(result.checks.length)} permission checks`}
                {result.kind === 'transaction'
                  ? result.committed
                    ? ' · committed'
                    : ' · dry run'
                  : ''}
              </p>

              {result.kind === 'query' ? (
                <section className="flex flex-col gap-1.5">
                  <h3 className="text-section">Result</h3>
                  <JsonTree
                    aria-label="Query result"
                    value={result.result}
                    className="max-h-64"
                  />
                </section>
              ) : result.allChecksOk ? (
                <p className="text-success text-sm">All checks passed.</p>
              ) : (
                <p className="text-destructive text-sm">
                  Some checks did not pass.
                </p>
              )}

              <section className="flex flex-col gap-1.5">
                <h3 className="text-section">Checks</h3>
                <SimpleTable
                  caption="Permission checks"
                  columns={columns}
                  rows={result.checks.map(
                    (row, index): CheckRow => ({
                      ...row,
                      key: [
                        row.namespace,
                        row.entityId ?? 'none',
                        row.action,
                        String(index),
                      ].join('-'),
                    }),
                  )}
                  getRowId={(row) => row.key}
                />
              </section>
            </div>
          ) : null}
        </SheetBody>

        <SheetFooter>
          <Button
            type="button"
            variant="secondary"
            onClick={() => {
              onOpenChange(false);
            }}
          >
            Close
          </Button>
          <Button
            type="button"
            disabled={running}
            onClick={() => {
              void run();
            }}
          >
            <Play aria-hidden="true" />
            {running ? 'Running…' : 'Run'}
          </Button>
        </SheetFooter>
      </SheetContent>
    </Sheet>
  );
}
