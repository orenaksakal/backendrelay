import {
  FormEvent,
  ReactNode,
  useContext,
  useEffect,
  useRef,
  useState,
} from 'react';
import {
  ArrowDownTrayIcon,
  CommandLineIcon,
  EllipsisVerticalIcon,
  TrashIcon,
} from '@heroicons/react/24/outline';

import config, { dashboardApiURI } from '@/lib/config';
import { useAuthedFetch } from '@/lib/auth';
import { jsonFetch, jsonMutate } from '@/lib/fetch';
import { errorToast, successToast } from '@/lib/toast';
import { TokenContext } from '@/lib/contexts';
import { InstantApp, InstantAppBackup, InstantAppBackupJob } from '@/lib/types';

import {
  Button,
  Content,
  Dialog,
  Label,
  SectionHeading,
  SubsectionHeading,
  TextInput,
  Tooltip,
  TooltipContent,
  TooltipTrigger,
  useDialog,
} from '@/components/ui';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/DropdownMenu';
import { formatTimestamp } from '@/components/dash/shared';
import {
  BackupSchedule,
  type BackupScheduleValue,
} from './backups/BackupSchedule';
import './backups/backups.css';
import { backupExpiry } from './backups/model';
import { useBackupDownloads } from '@/components/dash/BackupDownloadDialog';
import { useFetchedDash } from '@/components/dash/MainDashLayout';

function createBackup(token: string, appId: string, description?: string) {
  return jsonMutate<{ job: InstantAppBackupJob }>(
    `${config.apiURI}/dash/apps/${appId}/backups`,
    { token, body: description ? { description } : {} },
  );
}

function fetchBackupJob(token: string, appId: string, jobId: string) {
  return jsonFetch(`${config.apiURI}/dash/apps/${appId}/backup-jobs/${jobId}`, {
    headers: { authorization: `Bearer ${token}` },
  }) as Promise<{ job: InstantAppBackupJob | null }>;
}

function cancelBackupJob(token: string, appId: string, jobId: string) {
  return jsonMutate<{ id: string }>(
    `${config.apiURI}/dash/apps/${appId}/backup-jobs/${jobId}`,
    { token, method: 'DELETE' },
  );
}

// Active jobs and stored snapshots share a compact, horizontal resource row so
// history stays scannable even when the app has accumulated many snapshots.
function BackupItem({
  title,
  description,
  rows,
  action,
  corner,
}: {
  title: string;
  description?: string | null;
  rows: [string, ReactNode][];
  action: ReactNode;
  corner?: ReactNode;
}) {
  return (
    <div className="group relative flex flex-wrap items-center gap-x-6 gap-y-3 px-4 py-3 sm:flex-nowrap">
      {corner ? <div className="absolute top-2 right-2">{corner}</div> : null}
      <div className="min-w-0 flex-1">
        <p className="font-medium">{title}</p>
        {description ? (
          <p className="text-ui-muted text-sm">{description}</p>
        ) : null}
      </div>
      <dl className="text-ui-muted flex flex-wrap items-center gap-x-5 gap-y-1 text-xs">
        {rows.map(([label, value]) => [
          <div key={label} className="flex items-center gap-1.5">
            <dt>{label}</dt>
            <dd className="text-ui-ink">{value}</dd>
          </div>,
        ])}
      </dl>
      <div className="ml-auto flex shrink-0 items-center gap-2">{action}</div>
    </div>
  );
}

function ProgressBar({ pct }: { pct: number }) {
  return (
    <div className="bg-ui-subtle h-1.5 w-full max-w-[12rem] overflow-hidden rounded-full">
      <div
        className="bg-ui-accent h-full rounded-full transition-[width] duration-150"
        style={{ width: `${pct}%` }}
      />
    </div>
  );
}

function BackupJobRow({
  app,
  job,
  onCancelled,
}: {
  app: InstantApp;
  job: InstantAppBackupJob;
  onCancelled: () => void;
}) {
  const token = useContext(TokenContext);
  const confirmDialog = useDialog();
  const [cancelling, setCancelling] = useState(false);
  const completed = job.work_completed ?? 0;
  const estimate = job.work_estimate ?? 0;
  const pct =
    estimate > 0
      ? Math.min(99, Math.round((completed / estimate) * 100))
      : null;
  const label = job.job_status === 'waiting' ? 'Starting…' : 'Backing up…';

  async function onCancel() {
    if (!token) return;
    setCancelling(true);
    try {
      await cancelBackupJob(token, app.id, job.id);
      successToast('Backup cancelled.');
      confirmDialog.onClose();
      // The server drops the job from the active list immediately, so refetch to
      // clear the row (a running worker aborts at its next progress checkpoint).
      onCancelled();
    } catch (e: any) {
      errorToast(e?.body?.message ?? 'Failed to cancel backup.');
      setCancelling(false);
    }
  }

  return (
    <BackupItem
      title={formatTimestamp(job.created_at)}
      description={job.description}
      rows={[
        ['Status', cancelling ? 'Cancelling…' : label],
        [
          'Progress',
          <div className="flex items-center gap-2">
            <ProgressBar pct={pct ?? 0} />
            <span className="tabular-nums">
              {pct !== null ? `~${pct}%` : 'Estimating…'}
            </span>
          </div>,
        ],
      ]}
      action={
        <>
          <Button
            variant="secondary"
            size="mini"
            disabled={cancelling}
            onClick={confirmDialog.onOpen}
          >
            Cancel
          </Button>
          <Dialog title="Cancel backup" {...confirmDialog}>
            <div className="flex flex-col gap-4">
              <SubsectionHeading>Cancel backup</SubsectionHeading>
              <Content className="text-ui-muted text-sm">
                Cancel this backup? Progress so far will be discarded and no
                snapshot will be saved.
              </Content>
              <div className="flex flex-row gap-2">
                <Button
                  type="button"
                  variant="destructive"
                  loading={cancelling}
                  onClick={onCancel}
                >
                  Cancel backup
                </Button>
                <Button
                  type="button"
                  variant="secondary"
                  disabled={cancelling}
                  onClick={confirmDialog.onClose}
                >
                  Keep backing up
                </Button>
              </div>
            </div>
          </Dialog>
        </>
      }
    />
  );
}

function BackupRow({
  app,
  backup,
  onDeleted,
}: {
  app: InstantApp;
  backup: InstantAppBackup;
  onDeleted: () => void;
}) {
  const token = useContext(TokenContext);
  const dash = useFetchedDash();
  const { open, active } = useBackupDownloads();
  const deleteDialog = useDialog();
  const [deleting, setDeleting] = useState(false);
  const [deleteError, setDeleteError] = useState<string | null>(null);
  // Deleting is admin-only (the server enforces this too); hide the affordance
  // for everyone else.
  const orgRole =
    dash.data.workspace.type === 'org' ? dash.data.workspace.org.role : null;
  const canDelete =
    app.user_app_role === 'admin' ||
    app.user_app_role === 'owner' ||
    orgRole === 'admin' ||
    orgRole === 'owner';

  function openDeleteDialog() {
    setDeleteError(null);
    deleteDialog.onOpen();
  }

  // The equivalent `instant-cli backup download` invocation, for scripting or
  // downloading outside the browser. When the dashboard isn't pointed at the
  // CLI's default (production) backend — i.e. dev, staging, or a self-hosted
  // instance — prefix the exact apiURI via INSTANT_CLI_API_URI so the CLI hits
  // the same server this dashboard does.
  const cliDownloadCommand = (() => {
    const cmd = `npx instant-cli@latest backup download ${backup.id} --app ${app.id}`;
    return dashboardApiURI === 'https://api.instantdb.com'
      ? cmd
      : `INSTANT_CLI_API_URI='${dashboardApiURI}' ${cmd}`;
  })();

  async function copyCliDownloadCommand() {
    try {
      await window.navigator.clipboard.writeText(cliDownloadCommand);
      successToast('Copied CLI download command.');
    } catch {
      errorToast('Failed to copy to clipboard.');
    }
  }

  async function deleteBackup() {
    if (!token) return;
    setDeleting(true);
    setDeleteError(null);
    try {
      await jsonMutate(
        `${config.apiURI}/dash/apps/${app.id}/backups/${backup.id}`,
        { token, method: 'DELETE' },
      );
      deleteDialog.onClose();
      successToast('Backup deleted.');
      onDeleted();
    } catch (e: any) {
      setDeleteError(e?.body?.message ?? 'Failed to delete backup.');
    } finally {
      setDeleting(false);
    }
  }

  const isActive = active?.id === backup.id;
  // Only one download at a time: while another backup is actively downloading,
  // this row's button is disabled with an explanation.
  const blockedByOther = active?.status === 'downloading' && !isActive;
  // Only relabel once the download has actually started (past the confirm
  // dialog) — while confirming it's still just "Download".
  const downloadLabel =
    isActive && active?.status === 'downloading'
      ? 'Downloading…'
      : isActive &&
          (active?.status === 'complete' || active?.status === 'error')
        ? 'Show download'
        : 'Download';

  const downloadButton = (
    <Button
      variant="secondary"
      size="mini"
      disabled={blockedByOther}
      // pointer-events-none so hover falls through to the wrapping tooltip
      // trigger below — a disabled button emits no pointer events itself.
      className={blockedByOther ? 'pointer-events-none' : undefined}
      onClick={blockedByOther ? undefined : () => open(app, backup, token)}
    >
      <ArrowDownTrayIcon width={14} /> {downloadLabel}
    </Button>
  );

  // Keep recovery actions visible for mouse, touch, and keyboard users.
  const actionsMenu = (
    <DropdownMenu>
      <DropdownMenuTrigger
        aria-label={`Actions for snapshot ${backup.id}`}
        className="text-ui-muted hover:text-ui-ink cursor-pointer rounded p-1"
      >
        <EllipsisVerticalIcon width={16} />
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end">
        <DropdownMenuItem
          className="cursor-pointer"
          onSelect={async () => {
            try {
              await navigator.clipboard.writeText(backup.id);
              successToast('Snapshot ID copied.');
            } catch {
              errorToast('Failed to copy to clipboard.');
            }
          }}
        >
          Copy snapshot ID
        </DropdownMenuItem>
        <DropdownMenuItem
          className="cursor-pointer"
          onSelect={copyCliDownloadCommand}
        >
          <CommandLineIcon width={14} /> Copy CLI download command
        </DropdownMenuItem>
        {canDelete ? (
          <>
            <DropdownMenuSeparator className="bg-ui-line" />
            <DropdownMenuItem
              className="text-ui-danger focus:text-ui-danger cursor-pointer"
              onSelect={openDeleteDialog}
            >
              <TrashIcon width={14} className="text-ui-danger" /> Delete backup
            </DropdownMenuItem>
          </>
        ) : null}
      </DropdownMenuContent>
    </DropdownMenu>
  );

  return (
    <>
      <div className="backup-actions">
        {blockedByOther ? (
          <Tooltip delayDuration={0}>
            <TooltipTrigger asChild>
              <span className="inline-flex cursor-not-allowed">
                {downloadButton}
              </span>
            </TooltipTrigger>
            <TooltipContent>
              You can only download one backup at a time.
            </TooltipContent>
          </Tooltip>
        ) : (
          downloadButton
        )}
        {actionsMenu}
      </div>
      <Dialog title="Delete backup" {...deleteDialog}>
        <div className="flex flex-col gap-3">
          <SubsectionHeading>Delete backup</SubsectionHeading>
          <Content className="text-ui-muted text-sm">
            This permanently removes this snapshot. Download it first if you may
            need it later.
          </Content>
          {deleteError ? (
            <div className="bg-ui-accent-soft text-ui-danger rounded-sm p-3 text-sm">
              {deleteError}
            </div>
          ) : null}
          <div className="flex gap-2">
            <Button
              variant="destructive"
              loading={deleting}
              onClick={deleteBackup}
            >
              Delete backup
            </Button>
            <Button variant="secondary" onClick={deleteDialog.onClose}>
              Cancel
            </Button>
          </div>
        </div>
      </Dialog>
    </>
  );
}

export function Backups({ app }: { app: InstantApp }) {
  return <BackupsController key={app.id} app={app} />;
}

function BackupsController({ app }: { app: InstantApp }) {
  const token = useContext(TokenContext);
  const dash = useFetchedDash();
  const orgRole =
    dash.data.workspace.type === 'org' ? dash.data.workspace.org.role : null;
  const canEditSchedule =
    ['owner', 'admin'].includes(app.user_app_role ?? '') ||
    (!!orgRole && ['owner', 'admin'].includes(orgRole));
  const scheduleRes = useAuthedFetch<{ schedule: BackupScheduleValue | null }>(
    `${config.apiURI}/dash/apps/${app.id}/backup-schedule`,
  );
  const createDialog = useDialog();
  const [description, setDescription] = useState('');
  const [creating, setCreating] = useState(false);
  const [createError, setCreateError] = useState<string | null>(null);
  const [createErrorPhase, setCreateErrorPhase] = useState<
    'hidden' | 'entering' | 'visible' | 'leaving'
  >('hidden');
  // Retains the message while the error box fades out (createError is null
  // during the leave transition), so it doesn't blank before it's gone.
  const lastCreateError = useRef('');

  useEffect(() => {
    if (createError) {
      setCreateErrorPhase('entering');
      const frame = requestAnimationFrame(() => setCreateErrorPhase('visible'));
      return () => cancelAnimationFrame(frame);
    }

    if (createErrorPhase === 'hidden') return;
    setCreateErrorPhase('leaving');
    const timeout = setTimeout(() => setCreateErrorPhase('hidden'), 200);
    return () => clearTimeout(timeout);
  }, [createError]);

  function openCreateDialog() {
    setCreateError(null);
    createDialog.onOpen();
  }

  const backupsRes = useAuthedFetch<{
    backups: InstantAppBackup[];
  }>(`${config.apiURI}/dash/apps/${app.id}/backups`);

  const jobsRes = useAuthedFetch<{
    jobs: InstantAppBackupJob[];
  }>(`${config.apiURI}/dash/apps/${app.id}/backup-jobs`, {
    refreshInterval: (data) => (data?.jobs.length ? 1000 : 0),
  });

  const jobs = jobsRes.data?.jobs ?? [];
  const activeCount = jobs.length;
  const hasActive = activeCount > 0;
  const jobIds = jobs.map((j) => j.id);
  const jobIdsKey = jobIds.join(',');

  // Detect when a job we were watching leaves the active list and alert on its
  // outcome. `seenJobIds` starts empty, so we never alert for jobs that were
  // already finished before this component mounted.
  const seenJobIds = useRef<Set<string>>(new Set());
  useEffect(() => {
    // Rebuild the id set from jobIdsKey (not jobIds) so the effect keys off the
    // stable string and re-runs only when the set of active jobs changes.
    const current = new Set(jobIdsKey ? jobIdsKey.split(',') : []);
    const finished = [...seenJobIds.current].filter((id) => !current.has(id));
    seenJobIds.current = current;
    if (finished.length === 0 || !token) return;
    backupsRes.mutate();
    finished.forEach(async (id) => {
      try {
        const { job } = await fetchBackupJob(token, app.id, id);
        if (job?.job_status === 'completed') {
          successToast('Backup finished.');
        } else if (job?.job_status === 'errored') {
          errorToast('Backup failed.', { autoClose: 8000 });
        }
      } catch {
        // Best-effort alert; ignore lookup failures.
      }
    });
  }, [jobIdsKey, token, app.id, backupsRes.mutate]);

  const [now, setNow] = useState(Date.now);
  const [refreshing, setRefreshing] = useState(false);
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 60000);
    return () => clearInterval(timer);
  }, []);
  async function refresh() {
    setRefreshing(true);
    try {
      await Promise.all([
        backupsRes.mutate(),
        jobsRes.mutate(),
        scheduleRes.mutate(),
      ]);
    } catch {
      errorToast('Could not refresh backup status. Try again.');
    } finally {
      setNow(Date.now());
      setRefreshing(false);
    }
  }

  async function onCreate(e: FormEvent) {
    e.preventDefault();
    if (!token) return;
    setCreating(true);
    setCreateError(null);
    try {
      const desc = description.trim();
      const { job } = await createBackup(token, app.id, desc || undefined);
      await jobsRes.mutate(
        (current) => ({
          jobs: [
            job,
            ...(current?.jobs ?? []).filter(
              (existing) => existing.id !== job.id,
            ),
          ],
        }),
        false,
      );
      successToast('Backup started.');
      setDescription('');
      createDialog.onClose();
    } catch (e: any) {
      // Show the error (e.g. rate limit, too-large app) inline in the dialog so
      // it stays put next to the form instead of flashing by as a toast.
      const msg = e?.body?.message ?? 'Failed to start backup.';
      lastCreateError.current = msg;
      setCreateError(msg);
    } finally {
      setCreating(false);
    }
  }

  return (
    <div className="mx-auto flex w-full max-w-4xl flex-col gap-6 p-6">
      <div className="flex flex-wrap items-start justify-between gap-4">
        <div className="flex flex-col gap-1">
          <SectionHeading>Backups</SectionHeading>
          <Content className="text-sm">
            Point-in-time snapshots of your app's data.
          </Content>
        </div>
        <div className="flex gap-2">
          <Button
            size="mini"
            variant="secondary"
            loading={refreshing}
            onClick={refresh}
          >
            Refresh
          </Button>
          <Button
            size="mini"
            variant="primary"
            disabled={hasActive || jobsRes.isLoading || !!jobsRes.error}
            onClick={openCreateDialog}
          >
            Create backup
          </Button>
        </div>
      </div>
      {backupsRes.error || jobsRes.error ? (
        <p role="alert" className="text-ui-danger">
          Could not load backup status. Refresh to try again.
        </p>
      ) : backupsRes.isLoading || jobsRes.isLoading ? (
        <p role="status">Loading backups…</p>
      ) : (
        <div className="divide-y rounded-sm border bg-white dark:divide-neutral-700 dark:border-neutral-700 dark:bg-neutral-800">
          {jobs.map((job) => (
            <BackupJobRow
              key={job.id}
              app={app}
              job={job}
              onCancelled={() => jobsRes.mutate()}
            />
          ))}
          {(backupsRes.data?.backups ?? [])
            .filter((backup) => backupExpiry(backup, now) !== 'expired')
            .map((backup) => (
              <BackupRow
                key={backup.id}
                app={app}
                backup={backup}
                onDeleted={() => backupsRes.mutate()}
              />
            ))}
          {jobs.length === 0 &&
            !(backupsRes.data?.backups ?? []).some(
              (backup) => backupExpiry(backup, now) !== 'expired',
            ) && (
              <p className="p-8 text-center text-sm text-gray-500">
                No backups yet.
              </p>
            )}
        </div>
      )}
      <details>
        <summary className="cursor-pointer text-sm font-medium">
          Automatic backup schedule
        </summary>
        <div className="pt-4">
          <BackupSchedule
            schedule={scheduleRes.data?.schedule ?? null}
            loading={scheduleRes.isLoading}
            error={!!scheduleRes.error}
            canEdit={canEditSchedule}
            onRetry={() => {
              void scheduleRes.mutate();
            }}
            onSave={async (value) => {
              const result = await jsonFetch(
                `${config.apiURI}/dash/apps/${app.id}/backup-schedule`,
                {
                  method: 'PUT',
                  headers: {
                    authorization: `Bearer ${token}`,
                    'content-type': 'application/json',
                  },
                  body: JSON.stringify(value),
                },
              );
              await scheduleRes.mutate(result, { revalidate: false });
            }}
          />
        </div>
      </details>

      <Dialog title="Create backup" {...createDialog}>
        <form onSubmit={onCreate} className="flex flex-col gap-4">
          <SubsectionHeading>Create backup</SubsectionHeading>
          <Content className="text-ui-muted text-sm">
            Take a point-in-time snapshot of your app's data. Large apps can
            take a while.
          </Content>
          <div className="flex flex-col gap-1">
            <Label>Description (optional)</Label>
            <TextInput
              autoFocus
              value={description}
              onChange={setDescription}
              placeholder="e.g. Before migration"
            />
          </div>
          {createErrorPhase !== 'hidden' ? (
            <div
              className="bg-ui-accent-soft text-ui-danger max-h-40 overflow-hidden rounded-sm p-3 text-sm transition-[max-height,opacity,padding,transform] duration-300 ease-out data-[phase=entering]:max-h-0 data-[phase=entering]:-translate-y-1 data-[phase=entering]:p-0 data-[phase=entering]:opacity-0 data-[phase=leaving]:max-h-0 data-[phase=leaving]:-translate-y-1 data-[phase=leaving]:p-0 data-[phase=leaving]:opacity-0 data-[phase=leaving]:duration-200 data-[phase=leaving]:ease-in"
              data-phase={createErrorPhase}
            >
              {createError ?? lastCreateError.current}
            </div>
          ) : null}
          <div className="flex flex-row gap-2">
            <Button type="submit" variant="primary" loading={creating}>
              Create backup
            </Button>
            <Button
              type="button"
              variant="secondary"
              onClick={createDialog.onClose}
            >
              Cancel
            </Button>
          </div>
        </form>
      </Dialog>
    </div>
  );
}
