import { Link } from '@tanstack/react-router';
import { AlertTriangle, AppWindow, LayoutGrid, List, Star } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import { Bytes, Count } from '@/components/data/formatters';
import { IdChip } from '@/components/data/id-chip';
import { Pagination } from '@/components/data/pagination';
import { RelativeTime } from '@/components/data/relative-time';
import { SearchInput } from '@/components/data/search-input';
import {
  SimpleTable,
  type SimpleTableColumn,
} from '@/components/data/simple-table';
import { EmptyState } from '@/components/feedback/empty-state';
import { ErrorState } from '@/components/feedback/error-state';
import { StatusPill } from '@/components/feedback/status-pill';
import { TableSkeleton } from '@/components/feedback/skeletons';
import { PageHeader } from '@/components/layout/page-header';
import { Badge } from '@/components/ui/badge';
import { Button, buttonVariants } from '@/components/ui/button';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from '@/components/ui/tooltip';
import { isApiError } from '@/lib/api/client';
import {
  APPS_SORT_LABELS,
  APP_STATUS_FILTER_LABELS,
  APP_STATUS_FILTERS,
  backupIsStale,
  buildAppsHomeRows,
  effectiveAppStatus,
  filterAppsHomeRows,
  isAppsSortId,
  readAppsPageSize,
  sortAppsHomeRows,
  writeAppsPageSize,
  type AppsHomeRow,
  type AppsSortId,
  type AppStatusFilter,
  type SortDirection,
} from '@/lib/apps/apps-home';
import {
  readWorkspacePinnedAppIds,
  toggleWorkspacePinnedApp,
} from '@/lib/app-usage';
import { isPageSize, type PageSize } from '@/lib/account/preferences';
import { PAGE_ACTION_EVENT } from '@/lib/shortcuts';
import { cn } from '@/lib/utils';
import {
  useAppsSummary,
  useBootstrap,
  useOrgDetails,
} from '@/lib/workspace/queries';
import { roleForApp } from '@/lib/workspace/app-loader';
import { PERSONAL_WORKSPACE_ID } from '@/lib/workspace/workspaces';

export interface AppsHomePageProps {
  workspaceId: string;
  sort: AppsSortId;
  dir: SortDirection;
  /** Keeps the header sort buttons and `?sort=`/`?dir=` in step (C.4). */
  onSortChange: (sort: AppsSortId, dir: SortDirection) => void;
}

type AppsView = 'list' | 'grid';

const TABLE_COLUMNS = 9;
const STATUS_OPTIONS: readonly AppStatusFilter[] = APP_STATUS_FILTERS;

/**
 * C.4 apps home: every app in one workspace with sessions, sizes and the last
 * write and backup from the D0.2 summary. Pins, search, status, sort and the
 * page size are browser preferences; the workspace comes from the URL.
 */
export function AppsHomePage({
  workspaceId,
  sort,
  dir,
  onSortChange,
}: AppsHomePageProps) {
  const isPersonal = workspaceId === PERSONAL_WORKSPACE_ID;
  const bootstrap = useBootstrap();
  const org = useOrgDetails(isPersonal ? null : workspaceId);
  const summary = useAppsSummary(workspaceId);
  const toolbarRef = useRef<HTMLDivElement>(null);

  const [query, setQuery] = useState('');
  const [status, setStatus] = useState<AppStatusFilter>('all');
  const [view, setView] = useState<AppsView>('list');
  const [page, setPage] = useState(1);
  const [pageSize, setPageSize] = useState<PageSize>(() => readAppsPageSize());
  const [pinnedIds, setPinnedIds] = useState<string[]>(() =>
    readWorkspacePinnedAppIds(workspaceId),
  );

  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect -- switching workspace restores its pins from localStorage
    setPinnedIds(readWorkspacePinnedAppIds(workspaceId));
    setPage(1);
  }, [workspaceId]);

  // `/` focuses the page search instead of opening the command palette (B.7).
  useEffect(() => {
    const listener = (event: Event) => {
      const detail = (event as CustomEvent<{ action?: string } | undefined>)
        .detail;
      if (detail?.action !== 'search') return;
      const input = toolbarRef.current?.querySelector<HTMLInputElement>(
        'input[role="searchbox"]',
      );
      if (input === null || input === undefined) return;
      event.preventDefault();
      input.focus();
    };
    window.addEventListener(PAGE_ACTION_EVENT, listener);
    return () => {
      window.removeEventListener(PAGE_ACTION_EVENT, listener);
    };
  }, []);

  const appsError = isPersonal ? bootstrap.error : org.error;
  const error = appsError ?? summary.error;
  if (error !== null) {
    return (
      <div className="flex flex-col gap-4">
        <AppsHeader workspaceId={workspaceId} />
        <ErrorState
          thing="apps"
          status={isApiError(error) ? error.status : undefined}
          message={error instanceof Error ? error.message : undefined}
          requestId={isApiError(error) ? error.requestId : undefined}
          onRetry={() => {
            void (isPersonal ? bootstrap.refetch() : org.refetch());
            void summary.refetch();
          }}
        />
      </div>
    );
  }

  const apps = isPersonal ? bootstrap.data?.apps : org.data?.apps;
  const summaries = summary.data?.apps;
  if (apps === undefined || summaries === undefined) {
    return (
      <div className="flex flex-col gap-4">
        <AppsHeader workspaceId={workspaceId} />
        <TableSkeleton columns={TABLE_COLUMNS} />
      </div>
    );
  }

  const rows = buildAppsHomeRows(apps, summaries, pinnedIds);
  const filtered = filterAppsHomeRows(rows, query, status);
  const sorted = sortAppsHomeRows(filtered, sort, dir);
  const pageCount = Math.max(1, Math.ceil(sorted.length / pageSize));
  const safePage = Math.min(page, pageCount);
  const visible = sorted.slice((safePage - 1) * pageSize, safePage * pageSize);
  const filtering = query.trim() !== '' || status !== 'all';

  function changePageSize(next: number): void {
    if (!isPageSize(next)) return;
    setPageSize(next);
    writeAppsPageSize(next);
    setPage(1);
  }

  function clearFilters(): void {
    setQuery('');
    setStatus('all');
    setPage(1);
  }

  function togglePin(appId: string): void {
    setPinnedIds(toggleWorkspacePinnedApp(workspaceId, appId));
  }

  const columns: SimpleTableColumn<AppsHomeRow>[] = [
    {
      id: 'pin',
      header: <span className="sr-only">Pinned</span>,
      cell: (row) => (
        <PinButton
          row={row}
          onToggle={() => {
            togglePin(row.app.id);
          }}
        />
      ),
    },
    {
      id: 'name',
      header: 'Name',
      sortable: true,
      cell: (row) => (
        <span className="flex items-center gap-2">
          <Link
            to="/apps/$appId/overview"
            params={{ appId: row.app.id }}
            className="font-medium hover:underline"
          >
            {row.app.title}
          </Link>
          {roleForApp(row.app) === 'app-member' ? (
            <Badge tone="neutral">App member</Badge>
          ) : null}
        </span>
      ),
    },
    {
      id: 'id',
      header: 'App ID',
      cell: (row) => <IdChip value={row.app.id} />,
    },
    {
      id: 'status',
      header: 'Status',
      cell: (row) => <StatusPill status={effectiveAppStatus(row.app)} />,
    },
    {
      id: 'sessions',
      header: 'Sessions',
      align: 'right',
      sortable: true,
      cell: (row) =>
        row.metrics === undefined ? (
          <EmptyValue />
        ) : (
          <Count value={row.metrics.liveSessions} />
        ),
    },
    {
      id: 'data',
      header: 'Data',
      align: 'right',
      sortable: true,
      cell: (row) =>
        row.metrics === undefined ? (
          <EmptyValue />
        ) : (
          <Bytes value={row.metrics.appBytes} />
        ),
    },
    {
      id: 'files',
      header: 'Files',
      align: 'right',
      sortable: true,
      cell: (row) =>
        row.metrics === undefined ? (
          <EmptyValue />
        ) : (
          <Bytes value={row.metrics.storageBytes} />
        ),
    },
    {
      id: 'last-write',
      header: 'Last write',
      sortable: true,
      cell: (row) => <LastWrite value={row.metrics?.lastWriteAt ?? null} />,
    },
    {
      id: 'last-backup',
      header: 'Last backup',
      sortable: true,
      cell: (row) => <LastBackup value={row.metrics?.lastBackupAt ?? null} />,
    },
  ];

  return (
    <div className="flex flex-col gap-4">
      <AppsHeader workspaceId={workspaceId} showCreate={rows.length > 0} />

      {rows.length === 0 ? (
        <EmptyState
          icon={AppWindow}
          title="No apps yet"
          description="Create an app to get an App ID, a schema and live data."
          action={
            <Link
              to="/new"
              search={{ workspace: workspaceId }}
              className={cn(buttonVariants())}
            >
              Create app
            </Link>
          }
        />
      ) : (
        <>
          <div ref={toolbarRef} className="flex flex-wrap items-center gap-2">
            <SearchInput
              value={query}
              onValueChange={(value) => {
                setQuery(value);
                setPage(1);
              }}
              aria-label="Search apps"
              placeholder="Search apps…"
              showShortcut
              className="w-full sm:w-64"
            />

            <Select
              value={status}
              onValueChange={(value) => {
                if (!isStatusFilter(value)) return;
                setStatus(value);
                setPage(1);
              }}
            >
              <SelectTrigger aria-label="Filter by status" className="w-36">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {STATUS_OPTIONS.map((option) => (
                  <SelectItem key={option} value={option}>
                    {APP_STATUS_FILTER_LABELS[option]}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>

            <div className="flex items-center gap-1.5">
              <span
                aria-hidden="true"
                className="text-muted-foreground text-xs"
              >
                Sort
              </span>
              <Select
                value={sort}
                onValueChange={(value) => {
                  if (!isAppsSortId(value)) return;
                  setPage(1);
                  onSortChange(value, dir);
                }}
              >
                <SelectTrigger aria-label="Sort apps" className="w-40">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {Object.entries(APPS_SORT_LABELS).map(([id, label]) => (
                    <SelectItem key={id} value={id}>
                      {label}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>

            <div className="ml-auto flex items-center gap-1">
              <Button
                type="button"
                variant={view === 'list' ? 'secondary' : 'ghost'}
                size="sm"
                aria-label="List view"
                aria-pressed={view === 'list'}
                className="size-7 p-0"
                onClick={() => {
                  setView('list');
                }}
              >
                <List aria-hidden="true" />
              </Button>
              <Button
                type="button"
                variant={view === 'grid' ? 'secondary' : 'ghost'}
                size="sm"
                aria-label="Grid view"
                aria-pressed={view === 'grid'}
                className="size-7 p-0"
                onClick={() => {
                  setView('grid');
                }}
              >
                <LayoutGrid aria-hidden="true" />
              </Button>
            </div>
          </div>

          {view === 'grid' ? (
            <AppsGrid
              rows={visible}
              filtering={filtering}
              onTogglePin={togglePin}
              onClearFilters={clearFilters}
            />
          ) : (
            <SimpleTable
              caption="Apps"
              columns={columns}
              rows={visible}
              getRowId={(row) => row.app.id}
              sort={{ id: sort, dir }}
              onSortChange={(next) => {
                if (!isAppsSortId(next.id)) return;
                onSortChange(next.id, next.dir);
              }}
              empty={
                filtering ? (
                  <NoResults onClearFilters={clearFilters} />
                ) : (
                  'No apps'
                )
              }
            />
          )}

          <Pagination
            page={safePage}
            pageSize={pageSize}
            total={sorted.length}
            onPageChange={setPage}
            onPageSizeChange={changePageSize}
          />
        </>
      )}
    </div>
  );
}

function AppsHeader({
  workspaceId,
  showCreate = true,
}: {
  workspaceId: string;
  /** The empty state carries its own Create app button (C.4). */
  showCreate?: boolean;
}) {
  return (
    <PageHeader
      title="Apps"
      actions={
        showCreate ? (
          <Link
            to="/new"
            search={{ workspace: workspaceId }}
            className={cn(buttonVariants())}
          >
            Create app
          </Link>
        ) : undefined
      }
    />
  );
}

interface PinButtonProps {
  row: AppsHomeRow;
  onToggle: () => void;
}

function PinButton({ row, onToggle }: PinButtonProps) {
  const label = row.pinned ? `Unpin ${row.app.title}` : `Pin ${row.app.title}`;
  return (
    <Tooltip>
      <TooltipTrigger
        render={
          <Button
            type="button"
            variant="ghost"
            size="sm"
            aria-label={label}
            aria-pressed={row.pinned}
            className="size-7 p-0"
            onClick={onToggle}
          />
        }
      >
        <Star
          aria-hidden="true"
          className={row.pinned ? 'text-accent fill-current' : undefined}
        />
      </TooltipTrigger>
      <TooltipContent>{label}</TooltipContent>
    </Tooltip>
  );
}

function EmptyValue() {
  return <span className="text-muted-foreground">—</span>;
}

function LastWrite({ value }: { value: string | null }) {
  if (value === null) return <EmptyValue />;
  return <RelativeTime value={value} className="text-cell" />;
}

/** "never" (or an old date) carries a warning that explains the gap (C.4). */
function LastBackup({ value }: { value: string | null }) {
  return (
    <span className="inline-flex items-center gap-1.5">
      {value === null ? (
        <span className="text-muted-foreground">never</span>
      ) : (
        <RelativeTime value={value} className="text-cell" />
      )}
      {backupIsStale(value) ? <BackupWarning /> : null}
    </span>
  );
}

function BackupWarning() {
  return (
    <Tooltip>
      <TooltipTrigger
        render={
          <span className="text-warning inline-flex size-6 items-center justify-center rounded-sm" />
        }
      >
        <AlertTriangle aria-hidden="true" className="size-3.5" />
        <span className="sr-only">No backup in the last 7 days</span>
      </TooltipTrigger>
      <TooltipContent>No backup in the last 7 days</TooltipContent>
    </Tooltip>
  );
}

interface AppsGridProps {
  rows: readonly AppsHomeRow[];
  filtering: boolean;
  onTogglePin: (appId: string) => void;
  onClearFilters: () => void;
}

/** Cards, three per row at `xl`: title, status, sessions and last write. */
function AppsGrid({
  rows,
  filtering,
  onTogglePin,
  onClearFilters,
}: AppsGridProps) {
  if (rows.length === 0) {
    return filtering ? (
      <NoResults onClearFilters={onClearFilters} />
    ) : (
      <p className="text-muted-foreground text-sm">No apps.</p>
    );
  }

  return (
    <ul className="grid gap-4 sm:grid-cols-2 xl:grid-cols-3">
      {rows.map((row) => (
        <li
          key={row.app.id}
          className="border-border bg-card flex flex-col gap-3 rounded-lg border p-4"
        >
          <div className="flex items-start justify-between gap-2">
            <Link
              to="/apps/$appId/overview"
              params={{ appId: row.app.id }}
              className="text-section hover:underline"
            >
              {row.app.title}
            </Link>
            <PinButton
              row={row}
              onToggle={() => {
                onTogglePin(row.app.id);
              }}
            />
          </div>
          <StatusPill status={effectiveAppStatus(row.app)} />
          <dl className="grid grid-cols-2 gap-2 text-xs">
            <div className="flex flex-col gap-0.5">
              <dt className="text-muted-foreground">Sessions</dt>
              <dd>
                {row.metrics === undefined ? (
                  <EmptyValue />
                ) : (
                  <Count value={row.metrics.liveSessions} />
                )}
              </dd>
            </div>
            <div className="flex flex-col gap-0.5">
              <dt className="text-muted-foreground">Last write</dt>
              <dd>
                <LastWrite value={row.metrics?.lastWriteAt ?? null} />
              </dd>
            </div>
          </dl>
        </li>
      ))}
    </ul>
  );
}

function NoResults({ onClearFilters }: { onClearFilters: () => void }) {
  return (
    <span className="inline-flex flex-wrap items-center justify-center gap-1">
      No results for these filters.
      <Button type="button" variant="link" size="sm" onClick={onClearFilters}>
        Clear filters
      </Button>
    </span>
  );
}

function isStatusFilter(value: unknown): value is AppStatusFilter {
  return (
    typeof value === 'string' &&
    (STATUS_OPTIONS as readonly string[]).includes(value)
  );
}
