import { DatabaseBackup } from 'lucide-react';
import { useState } from 'react';
import { RelativeTime } from '@/components/data/relative-time';
import {
  SimpleTable,
  type SimpleTableColumn,
} from '@/components/data/simple-table';
import { ConfirmDialog } from '@/components/feedback/confirm-dialog';
import { EmptyState } from '@/components/feedback/empty-state';
import { ErrorState } from '@/components/feedback/error-state';
import { StatusPill } from '@/components/feedback/status-pill';
import { TableSkeleton } from '@/components/feedback/skeletons';
import { Button } from '@/components/ui/button';
import { isApiError } from '@/lib/api/client';
import type { BackupJob, RestoreJob } from '@/lib/api/endpoints';
import {
  backupJobPill,
  backupJobProgress,
  isActiveBackupJob,
  isActiveRestoreJob,
  jobDuration,
  restoreJobPill,
} from '@/lib/backups/model';
import {
  useAppRestoreJobs,
  useBackupJobs,
  useCancelBackupJob,
} from '@/lib/backups/queries';
import { truncateId } from '@/lib/format';
import { toastManager } from '@/lib/toast';
import { BackupProgress } from './backup-progress';

type JobRow =
  | { kind: 'backup'; job: BackupJob }
  | { kind: 'restore'; job: RestoreJob };

function jobCreatedAt(row: JobRow): string {
  return row.job.created_at;
}

function jobLabel(row: JobRow): string {
  if (row.kind === 'backup') {
    return row.job.description?.trim() || `Backup ${truncateId(row.job.id)}`;
  }
  return row.job.title?.trim() || `Restore ${truncateId(row.job.id)}`;
}

export interface BackupJobsTabProps {
  appId: string;
}

/** C.15 jobs tab: active backups and every restore for this app. */
export function BackupJobsTab({ appId }: BackupJobsTabProps) {
  const backups = useBackupJobs(appId);
  const restores = useAppRestoreJobs(appId);
  const cancel = useCancelBackupJob(appId);
  const [cancelTarget, setCancelTarget] = useState<BackupJob>();

  const rows: JobRow[] = [
    ...(backups.data?.jobs ?? []).map(
      (job): JobRow => ({ kind: 'backup', job }),
    ),
    ...(restores.data?.['restore-jobs'] ?? []).map(
      (job): JobRow => ({ kind: 'restore', job }),
    ),
  ].sort((a, b) => Date.parse(jobCreatedAt(b)) - Date.parse(jobCreatedAt(a)));

  const columns: SimpleTableColumn<JobRow>[] = [
    {
      id: 'job',
      header: 'Job',
      cell: (row) => (
        <div className="flex min-w-0 flex-col gap-0.5">
          <span className="truncate font-medium">{jobLabel(row)}</span>
          <span className="text-muted-foreground font-mono text-xs">
            {truncateId(row.job.id)}
          </span>
        </div>
      ),
    },
    {
      id: 'type',
      header: 'Type',
      cell: (row) => (row.kind === 'backup' ? 'Backup' : 'Restore'),
    },
    {
      id: 'status',
      header: 'Status',
      cell: (row) => {
        const pill =
          row.kind === 'backup'
            ? backupJobPill(row.job)
            : restoreJobPill(row.job);
        return <StatusPill status={pill.status} label={pill.label} />;
      },
    },
    {
      id: 'progress',
      header: 'Progress',
      cell: (row) =>
        row.kind === 'backup' ? (
          <BackupProgress
            value={backupJobProgress(row.job)}
            label={`Progress for ${jobLabel(row)}`}
          />
        ) : (
          <span className="text-cell tabular-nums">
            {row.job.progress ?? '—'}
          </span>
        ),
    },
    {
      id: 'started',
      header: 'Started',
      cell: (row) => (
        <RelativeTime value={row.job.created_at} className="text-cell" />
      ),
    },
    {
      id: 'duration',
      header: 'Duration',
      cell: (row) => (
        <span className="text-cell tabular-nums">{jobDuration(row.job)}</span>
      ),
    },
    {
      id: 'error',
      header: 'Error',
      cell: (row) =>
        row.kind === 'restore' && row.job.error ? (
          <span className="text-destructive text-xs">{row.job.error}</span>
        ) : (
          '—'
        ),
    },
    {
      id: 'actions',
      header: <span className="sr-only">Actions</span>,
      align: 'right',
      cell: (row) =>
        row.kind === 'backup' && isActiveBackupJob(row.job) ? (
          <Button
            type="button"
            variant="ghost"
            size="sm"
            aria-label={`Cancel backup job ${jobLabel(row)}`}
            onClick={() => {
              cancel.reset();
              setCancelTarget(row.job);
            }}
          >
            Cancel
          </Button>
        ) : row.kind === 'restore' && isActiveRestoreJob(row.job) ? (
          <span className="text-muted-foreground text-xs">
            Cancel from Operator → Restores
          </span>
        ) : null,
    },
  ];

  const bothFailed =
    backups.isError && backups.data === undefined && restores.isError;

  if (bothFailed) {
    return (
      <ErrorState
        thing="backup jobs"
        status={isApiError(backups.error) ? backups.error.status : undefined}
        message={
          backups.error instanceof Error ? backups.error.message : undefined
        }
        type={isApiError(backups.error) ? backups.error.type : undefined}
        hint={isApiError(backups.error) ? backups.error.hint : undefined}
        requestId={
          isApiError(backups.error) ? backups.error.requestId : undefined
        }
        onRetry={() => {
          void backups.refetch();
          void restores.refetch();
        }}
      />
    );
  }

  const loading = backups.isPending || restores.isPending;

  return (
    <div className="flex flex-col gap-3">
      <div className="flex justify-end">
        <Button
          type="button"
          variant="secondary"
          disabled={backups.isFetching || restores.isFetching}
          onClick={() => {
            void backups.refetch();
            void restores.refetch();
          }}
        >
          Refresh
        </Button>
      </div>

      {cancel.isError ? (
        <p role="alert" className="text-destructive text-sm">
          {cancel.error instanceof Error
            ? cancel.error.message
            : 'Could not cancel the backup.'}
        </p>
      ) : null}

      {loading ? (
        <TableSkeleton columns={8} />
      ) : rows.length === 0 ? (
        <EmptyState
          icon={DatabaseBackup}
          title="No backup jobs"
          description="Create a backup and its progress appears here until it finishes."
        />
      ) : (
        <SimpleTable
          caption="Backup jobs"
          columns={columns}
          rows={rows}
          getRowId={(row) => `${row.kind}:${row.job.id}`}
          empty="No backup jobs"
        />
      )}

      <ConfirmDialog
        open={cancelTarget !== undefined}
        onOpenChange={(open) => {
          if (!open) setCancelTarget(undefined);
        }}
        title={
          cancelTarget === undefined
            ? 'Cancel backup?'
            : `Cancel ${jobLabel({ kind: 'backup', job: cancelTarget })}?`
        }
        description="The worker stops at its next checkpoint. Progress so far is discarded and no snapshot is saved."
        confirmLabel="Cancel backup job"
        confirming={cancel.isPending}
        onConfirm={() => {
          if (cancelTarget === undefined) return;
          cancel.mutate(cancelTarget.id, {
            onSuccess: () => {
              setCancelTarget(undefined);
              toastManager.add({ type: 'success', title: 'Backup cancelled' });
            },
          });
        }}
      />
    </div>
  );
}
