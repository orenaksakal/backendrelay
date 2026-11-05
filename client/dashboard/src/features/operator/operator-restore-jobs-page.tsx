import { RotateCcw } from 'lucide-react';
import { useState } from 'react';
import { IdChip } from '@/components/data/id-chip';
import { KeyValueList } from '@/components/data/key-value-list';
import { RelativeTime } from '@/components/data/relative-time';
import {
  SimpleTable,
  type SimpleTableColumn,
} from '@/components/data/simple-table';
import { ConfirmDialog } from '@/components/feedback/confirm-dialog';
import { EmptyState } from '@/components/feedback/empty-state';
import { ErrorState } from '@/components/feedback/error-state';
import {
  StatusPill,
  type StatusPillStatus,
} from '@/components/feedback/status-pill';
import { TableSkeleton } from '@/components/feedback/skeletons';
import { PageHeader } from '@/components/layout/page-header';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { isApiError } from '@/lib/api/client';
import type { RestoreJob } from '@/lib/api/endpoints';
import { truncateId } from '@/lib/format';
import { creationErrorMessage } from '@/lib/creation/creation';
import { isRestoreJobActive, restoreJobLabel } from '@/lib/operator/model';
import { useCancelRestoreJob, useRestoreJobs } from '@/lib/operator/queries';
import { toastManager } from '@/lib/toast';
import { useBootstrap } from '@/lib/workspace/queries';

const CANCEL_ERROR = "Couldn't cancel the restore job. Try again.";

interface RestorePill {
  status: StatusPillStatus;
  label: string;
}

function restorePill(job: RestoreJob): RestorePill {
  switch (job.job_status) {
    case 'waiting':
      return { status: 'running', label: 'Waiting' };
    case 'processing':
      return { status: 'running', label: 'Running' };
    case 'done':
      return { status: 'active', label: 'Done' };
    case 'failed':
      return { status: 'failed', label: 'Failed' };
    case 'cancelled':
      return { status: 'disabled', label: 'Cancelled' };
    default:
      return { status: 'disabled', label: job.job_status };
  }
}

/**
 * C.19 Restores: every restore job the operator started, newest first, with
 * the worker's progress, a detail dialog and a cancel action while a job is
 * still queued or running.
 */
export function OperatorRestoresPage() {
  const jobs = useRestoreJobs();
  const cancel = useCancelRestoreJob();
  const bootstrap = useBootstrap();
  const [detail, setDetail] = useState<RestoreJob | undefined>();
  const [cancelTarget, setCancelTarget] = useState<RestoreJob | undefined>();

  const rows = jobs.data?.['restore-jobs'] ?? [];

  function appTitle(appId: string): string {
    return (
      bootstrap.data?.apps.find((app) => app.id === appId)?.title ??
      truncateId(appId)
    );
  }

  const columns: SimpleTableColumn<RestoreJob>[] = [
    {
      id: 'job',
      header: 'Job',
      cell: (row) => (
        <div className="flex min-w-0 flex-col gap-0.5">
          <span className="truncate font-medium">{restoreJobLabel(row)}</span>
          <span className="text-muted-foreground font-mono text-xs">
            {truncateId(row.id)}
          </span>
        </div>
      ),
    },
    {
      id: 'app',
      header: 'App',
      cell: (row) => <span className="text-cell">{appTitle(row.app_id)}</span>,
    },
    {
      id: 'status',
      header: 'Status',
      cell: (row) => {
        const pill = restorePill(row);
        return <StatusPill status={pill.status} label={pill.label} />;
      },
    },
    {
      id: 'progress',
      header: 'Progress',
      cell: (row) => (
        <span className="text-cell tabular-nums">{row.progress ?? '—'}</span>
      ),
    },
    {
      id: 'created',
      header: 'Created',
      cell: (row) => (
        <RelativeTime value={row.created_at} className="text-cell" />
      ),
    },
    {
      id: 'actions',
      header: <span className="sr-only">Actions</span>,
      align: 'right',
      cell: (row) => (
        <div className="flex items-center justify-end gap-1">
          <Button
            type="button"
            variant="ghost"
            size="sm"
            aria-label={`View restore job ${restoreJobLabel(row)}`}
            onClick={() => {
              setDetail(row);
            }}
          >
            View
          </Button>
          {isRestoreJobActive(row) ? (
            <Button
              type="button"
              variant="ghost"
              size="sm"
              aria-label={`Cancel restore job ${restoreJobLabel(row)}`}
              onClick={() => {
                cancel.reset();
                setCancelTarget(row);
              }}
            >
              Cancel
            </Button>
          ) : null}
        </div>
      ),
    },
  ];

  return (
    <div className="flex max-w-4xl flex-col gap-4">
      <PageHeader
        title="Restores"
        description="Restore jobs across every app. A job can be cancelled while it is waiting or running."
        actions={
          <Button
            type="button"
            variant="secondary"
            disabled={jobs.isFetching}
            onClick={() => {
              void jobs.refetch();
            }}
          >
            Refresh
          </Button>
        }
      />

      {jobs.isError && jobs.data === undefined ? (
        <ErrorState
          thing="restore jobs"
          status={isApiError(jobs.error) ? jobs.error.status : undefined}
          message={jobs.error instanceof Error ? jobs.error.message : undefined}
          type={isApiError(jobs.error) ? jobs.error.type : undefined}
          hint={isApiError(jobs.error) ? jobs.error.hint : undefined}
          requestId={isApiError(jobs.error) ? jobs.error.requestId : undefined}
          onRetry={() => {
            void jobs.refetch();
          }}
        />
      ) : jobs.isPending ? (
        <TableSkeleton columns={6} />
      ) : rows.length === 0 ? (
        <EmptyState
          icon={RotateCcw}
          title="No restore jobs"
          description="Restore a backup from an app's Backups screen and the job appears here."
        />
      ) : (
        <SimpleTable
          caption="Restore jobs"
          columns={columns}
          rows={rows}
          getRowId={(row) => row.id}
          loading={jobs.isPending}
          empty="No restore jobs"
        />
      )}

      {cancel.isError ? (
        <p role="alert" className="text-destructive text-sm">
          {creationErrorMessage(cancel.error, CANCEL_ERROR)}
        </p>
      ) : null}

      <Dialog
        open={detail !== undefined}
        onOpenChange={(open) => {
          if (!open) setDetail(undefined);
        }}
      >
        <DialogContent showCloseButton={false}>
          <DialogHeader>
            <DialogTitle>
              {detail === undefined ? 'Restore job' : restoreJobLabel(detail)}
            </DialogTitle>
            <DialogDescription>
              Details reported by the restore worker.
            </DialogDescription>
          </DialogHeader>
          {detail === undefined ? null : (
            <KeyValueList
              className="mt-4"
              items={[
                {
                  label: 'Status',
                  value: (
                    <StatusPill
                      status={restorePill(detail).status}
                      label={restorePill(detail).label}
                    />
                  ),
                },
                { label: 'Job ID', value: <IdChip value={detail.id} /> },
                {
                  label: 'App',
                  value: (
                    <span className="flex flex-wrap items-center gap-2">
                      {appTitle(detail.app_id)}
                      <IdChip value={detail.app_id} />
                    </span>
                  ),
                },
                {
                  label: 'Progress',
                  value: detail.progress ?? '—',
                },
                {
                  label: 'Error',
                  value:
                    detail.error === undefined || detail.error === null ? (
                      '—'
                    ) : (
                      <span className="text-destructive">{detail.error}</span>
                    ),
                },
                {
                  label: 'Created',
                  value: <RelativeTime value={detail.created_at} />,
                },
                {
                  label: 'Updated',
                  value: <RelativeTime value={detail.updated_at} />,
                },
                {
                  label: 'Finished',
                  value:
                    detail.done_at === undefined || detail.done_at === null ? (
                      '—'
                    ) : (
                      <RelativeTime value={detail.done_at} />
                    ),
                },
              ]}
            />
          )}
          <DialogFooter>
            <Button
              type="button"
              variant="secondary"
              onClick={() => {
                setDetail(undefined);
              }}
            >
              Close
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <ConfirmDialog
        open={cancelTarget !== undefined}
        onOpenChange={(open) => {
          if (!open) setCancelTarget(undefined);
        }}
        title={
          cancelTarget === undefined
            ? 'Cancel restore job?'
            : `Cancel restore of ${restoreJobLabel(cancelTarget)}?`
        }
        description="The worker stops. The new app keeps whatever it has restored so far."
        confirmLabel="Cancel restore job"
        confirming={cancel.isPending}
        onConfirm={() => {
          if (cancelTarget === undefined) return;
          cancel.mutate(cancelTarget.id, {
            onSuccess: () => {
              setCancelTarget(undefined);
              toastManager.add({ type: 'success', title: 'Restore cancelled' });
            },
          });
        }}
      />
    </div>
  );
}
