import { AlertTriangle, Check, Clock } from 'lucide-react';
import { useState, type ReactNode } from 'react';
import { Bytes, Count } from '@/components/data/formatters';
import { IdChip } from '@/components/data/id-chip';
import { RelativeTime } from '@/components/data/relative-time';
import {
  SimpleTable,
  type SimpleTableColumn,
} from '@/components/data/simple-table';
import { CardSkeleton, TableSkeleton } from '@/components/feedback/skeletons';
import { StatusPill } from '@/components/feedback/status-pill';
import { PageHeader } from '@/components/layout/page-header';
import { SectionCard } from '@/components/layout/section-card';
import { Button } from '@/components/ui/button';
import {
  Popover,
  PopoverContent,
  PopoverTitle,
  PopoverTrigger,
} from '@/components/ui/popover';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { isApiError } from '@/lib/api/client';
import type { App, EffectiveRole } from '@/lib/api/endpoints/schemas';
import { effectiveAppStatus } from '@/lib/apps/apps-home';
import {
  buildOverviewHealth,
  appHasCustomSender,
  lastSevenDays,
  sessionOriginRows,
  type DailyWrites,
  type HealthTone,
  type SessionOriginRow,
} from '@/lib/apps/overview';
import {
  useAppBackups,
  useAppBackupSchedule,
  useAppStats,
  useAppUsage,
  useSenderVerification,
} from '@/lib/apps/overview-queries';
import { copyAnnouncement, useCopyFeedback } from '@/lib/clipboard';
import { ErrorState } from '@/components/feedback/error-state';
import { appHref } from '@/lib/navigation';
import { getRuntimeConfig } from '@/lib/runtime-config';
import { websocketUrl } from '@/lib/urls';
import { ConnectSheet } from './connect-sheet';

export interface AppOverviewPageProps {
  app: App;
  role: EffectiveRole;
}

function canAdminApp(role: EffectiveRole): boolean {
  return role === 'owner' || role === 'admin';
}

/**
 * C.5 app overview: KPI cards, health checks and live-session origins, plus the
 * Connect sheet with the app's real IDs and endpoints.
 */
export function AppOverviewPage({ app, role }: AppOverviewPageProps) {
  const usage = useAppUsage(app.id);
  const stats = useAppStats(app.id);
  const backups = useAppBackups(app.id);
  const schedule = useAppBackupSchedule(app.id);
  const canAdmin = canAdminApp(role);
  const sender = useSenderVerification(app.id, canAdmin);
  const [connectOpen, setConnectOpen] = useState(false);
  const { status: copyStatus, copy } = useCopyFeedback();

  const config = getRuntimeConfig();
  const apiUrl = config?.apiBaseUrl ?? '';
  const realtimeUrl = apiUrl === '' ? undefined : websocketUrl(apiUrl);

  const checks = buildOverviewHealth({
    appId: app.id,
    rules: app.rules,
    webhooks: app.webhooks ?? [],
    canAdmin,
    hasCustomSender: appHasCustomSender(app),
    senderVerified: sender.data?.instant['verified?'] ?? undefined,
    senderLoaded: !canAdmin || !sender.isPending,
    lastBackupAt: backups.data?.backups[0]?.backup_at ?? null,
    backupsLoaded: backups.data !== undefined || backups.isError,
    backupsFailed: backups.isError,
    hasBackupSchedule: schedule.data?.schedule?.enabled === true,
    scheduleLoaded: schedule.data !== undefined || schedule.isError,
  });

  const origins = sessionOriginRows(stats.data);
  const daily = lastSevenDays(usage.data?.transactions.daily ?? []);
  const originColumns: SimpleTableColumn<SessionOriginRow>[] = [
    {
      id: 'origin',
      header: 'Origin',
      cell: (row) => (
        <span className="text-cell font-mono break-all">{row.origin}</span>
      ),
    },
    {
      id: 'sessions',
      header: 'Sessions',
      align: 'right',
      cell: (row) => <Count value={row.sessions} />,
    },
  ];

  return (
    <div className="flex flex-col gap-4">
      <PageHeader
        title={app.title}
        description={
          <span className="flex flex-wrap items-center gap-2">
            <IdChip value={app.id} />
            <StatusPill status={effectiveAppStatus(app)} />
            <span>
              Created <RelativeTime value={app.created_at} />
            </span>
          </span>
        }
        actions={
          <>
            <Button
              type="button"
              onClick={() => {
                setConnectOpen(true);
              }}
            >
              Connect
            </Button>
            <DropdownMenu>
              <DropdownMenuTrigger
                render={<Button type="button" variant="secondary" />}
              >
                Actions
              </DropdownMenuTrigger>
              <DropdownMenuContent align="end">
                <DropdownMenuItem
                  onClick={() => {
                    void copy(app.id);
                  }}
                >
                  Copy app ID
                </DropdownMenuItem>
                <DropdownMenuSeparator />
                <DropdownMenuItem
                  render={
                    <a
                      href={appHref(app.id, 'settings')}
                      aria-label="Settings"
                    />
                  }
                >
                  Settings
                </DropdownMenuItem>
              </DropdownMenuContent>
            </DropdownMenu>
          </>
        }
      />

      <span role="status" className="sr-only">
        {copyAnnouncement(copyStatus)}
      </span>

      {usage.isError ? (
        <ErrorState
          thing="app metrics"
          status={isApiError(usage.error) ? usage.error.status : undefined}
          message={
            usage.error instanceof Error ? usage.error.message : undefined
          }
          requestId={
            isApiError(usage.error) ? usage.error.requestId : undefined
          }
          onRetry={() => {
            void usage.refetch();
          }}
        />
      ) : (
        <section
          aria-label="App metrics"
          className="grid gap-4 sm:grid-cols-2 xl:grid-cols-4"
        >
          <KpiCard
            title="Live sessions"
            loading={usage.isPending || stats.isPending}
          >
            {stats.isError ? (
              <MetricUnavailable
                onRetry={() => {
                  void stats.refetch();
                }}
              />
            ) : (
              <>
                <MetricValue>
                  <Count value={stats.data?.count ?? 0} />
                </MetricValue>
                <p className="text-muted-foreground text-xs">
                  {origins.length === 0 ? (
                    'No origins recorded'
                  ) : (
                    <Popover>
                      <PopoverTrigger
                        render={
                          <Button
                            type="button"
                            variant="link"
                            size="sm"
                            className="h-auto p-0 text-xs"
                          />
                        }
                      >
                        {`${String(origins.length)} ${origins.length === 1 ? 'origin' : 'origins'}`}
                      </PopoverTrigger>
                      <PopoverContent>
                        <PopoverTitle>Live session origins</PopoverTitle>
                        <ul className="mt-2 flex flex-col gap-1.5">
                          {origins.map((origin) => (
                            <li
                              key={origin.origin}
                              className="flex items-center justify-between gap-3 text-sm"
                            >
                              <span className="text-cell min-w-0 font-mono break-all">
                                {origin.origin}
                              </span>
                              <Count value={origin.sessions} />
                            </li>
                          ))}
                        </ul>
                      </PopoverContent>
                    </Popover>
                  )}
                </p>
              </>
            )}
          </KpiCard>

          <KpiCard title="Writes (24h)" loading={usage.isPending}>
            <MetricValue>
              <Count value={usage.data?.transactions.last24h ?? 0} />
            </MetricValue>
            <Sparkline points={daily} />
          </KpiCard>

          <KpiCard title="Data size" loading={usage.isPending}>
            <MetricValue>
              <Bytes value={usage.data?.appBytes ?? 0} />
            </MetricValue>
            <p className="text-muted-foreground text-xs">Database storage</p>
          </KpiCard>

          <KpiCard title="File storage" loading={usage.isPending}>
            <MetricValue>
              <Bytes value={usage.data?.storageBytes ?? 0} />
            </MetricValue>
            <p className="text-muted-foreground text-xs">
              {usage.data?.fileCount === null ||
              usage.data?.fileCount === undefined
                ? 'Stored files'
                : `${String(usage.data.fileCount)} ${usage.data.fileCount === 1 ? 'file' : 'files'}`}
            </p>
          </KpiCard>
        </section>
      )}

      <div className="grid gap-4 lg:grid-cols-2">
        <SectionCard title="Health">
          <ul className="flex flex-col gap-3">
            {checks.map((check) => (
              <li key={check.id} className="flex items-start gap-2">
                <HealthIcon tone={check.tone} />
                <div className="flex min-w-0 flex-1 flex-col gap-0.5">
                  <p className="text-sm">{check.text}</p>
                  {check.detail === undefined ? null : (
                    <p className="text-muted-foreground text-xs">
                      {check.detail}
                    </p>
                  )}
                </div>
                <a
                  href={check.href}
                  className="text-accent shrink-0 text-xs hover:underline"
                >
                  {check.actionLabel}
                </a>
              </li>
            ))}
          </ul>
        </SectionCard>

        <SectionCard
          title="Activity"
          description="Where live sessions connect from."
        >
          {stats.isError ? (
            <p className="text-muted-foreground flex flex-wrap items-center gap-2 text-sm">
              Live session origins are unavailable.
              <Button
                type="button"
                variant="link"
                size="sm"
                onClick={() => {
                  void stats.refetch();
                }}
              >
                Retry
              </Button>
            </p>
          ) : stats.isPending ? (
            <TableSkeleton rows={3} columns={2} />
          ) : (
            <SimpleTable
              caption="Live session origins"
              columns={originColumns}
              rows={origins}
              getRowId={(row) => row.origin}
              empty="No connected sessions"
            />
          )}
        </SectionCard>
      </div>

      <ConnectSheet
        app={app}
        open={connectOpen}
        onOpenChange={setConnectOpen}
        details={{
          appId: app.id,
          apiUrl,
          websocketUrl: realtimeUrl ?? '',
          dashboardUrl:
            typeof window === 'undefined' ? '' : window.location.origin,
        }}
      />
    </div>
  );
}

function KpiCard({
  title,
  loading,
  children,
}: {
  title: string;
  loading: boolean;
  children: ReactNode;
}) {
  if (loading) return <CardSkeleton />;
  return (
    <div className="border-border bg-card flex flex-col gap-2 rounded-lg border p-4">
      <p className="text-muted-foreground text-xs">{title}</p>
      {children}
    </div>
  );
}

function MetricValue({ children }: { children: ReactNode }) {
  return <p className="text-2xl font-semibold tabular-nums">{children}</p>;
}

function MetricUnavailable({ onRetry }: { onRetry: () => void }) {
  return (
    <div className="flex flex-wrap items-center gap-2">
      <span className="text-muted-foreground text-sm">Unavailable</span>
      <Button type="button" variant="link" size="sm" onClick={onRetry}>
        Retry
      </Button>
    </div>
  );
}

function HealthIcon({ tone }: { tone: HealthTone }) {
  if (tone === 'warning') {
    return (
      <AlertTriangle
        className="text-warning mt-0.5 size-4 shrink-0"
        aria-hidden="true"
      />
    );
  }
  if (tone === 'pending') {
    return (
      <Clock
        className="text-muted-foreground mt-0.5 size-4 shrink-0"
        aria-hidden="true"
      />
    );
  }
  return (
    <Check className="text-success mt-0.5 size-4 shrink-0" aria-hidden="true" />
  );
}

function Sparkline({ points }: { points: ReadonlyArray<DailyWrites> }) {
  if (points.length === 0) return null;
  const max = Math.max(1, ...points.map((point) => point.count));
  const step = points.length > 1 ? 100 / (points.length - 1) : 0;
  const coordinates = points
    .map((point, index) => {
      const x = index * step;
      const y = 22 - (point.count / max) * 18;
      return `${x.toFixed(2)},${y.toFixed(2)}`;
    })
    .join(' ');

  return (
    <svg
      viewBox="0 0 100 24"
      preserveAspectRatio="none"
      className="h-6 w-full"
      aria-hidden="true"
    >
      <polyline
        points={coordinates}
        fill="none"
        stroke="var(--chart-1)"
        strokeWidth="1.5"
        vectorEffect="non-scaling-stroke"
      />
    </svg>
  );
}
