import { useState } from 'react';
import {
  DatabaseBackup,
  Download,
  Ellipsis,
  Plus,
  RotateCcw,
  Trash2,
} from 'lucide-react';
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
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { Input } from '@/components/ui/input';
import { isApiError } from '@/lib/api/client';
import type { App, EffectiveRole } from '@/lib/api/endpoints/schemas';
import type { Backup } from '@/lib/api/endpoints';
import {
  backupDisplaySize,
  backupExpiry,
  canCreateBackup,
  canDeleteBackup,
  canRestoreBackup,
  createBackupErrorMessage,
  visibleBackups,
} from '@/lib/backups/model';
import {
  useBackups,
  useBackupJobs,
  useCreateBackup,
  useDeleteBackup,
  useRefreshBackupsOnJobCompletion,
} from '@/lib/backups/queries';
import { toastManager } from '@/lib/toast';
import { BackupDownloadSheet } from './backup-download-sheet';
import { RestoreBackupSheet } from './restore-backup-sheet';

export interface SnapshotsTabProps {
  app: App;
  role: EffectiveRole;
}

function snapshotLabel(backup: Backup): string {
  return backup.description?.trim() || `Snapshot from ${backup.backup_at}`;
}

function expiryPill(backup: Backup) {
  switch (backupExpiry(backup)) {
    case 'expiring':
      return <StatusPill status="read-only" label="Expires soon" />;
    case 'retained':
      return <StatusPill status="active" label="Retained" />;
    case 'available':
      return <StatusPill status="active" label="Available" />;
    default:
      return <StatusPill status="disabled" label="Unknown" />;
  }
}

/** C.15 snapshots: the stored snapshots, create/delete and the row actions. */
export function SnapshotsTab({ app, role }: SnapshotsTabProps) {
  const jobs = useBackupJobs(app.id);
  const hasActiveJob = (jobs.data?.jobs.length ?? 0) > 0;
  useRefreshBackupsOnJobCompletion(app.id, hasActiveJob);
  const backups = useBackups(app.id, hasActiveJob);
  const create = useCreateBackup(app.id);
  const remove = useDeleteBackup(app.id);
  const [createOpen, setCreateOpen] = useState(false);
  const [description, setDescription] = useState('');
  const [createError, setCreateError] = useState<string>();
  const [deleteTarget, setDeleteTarget] = useState<Backup>();
  const [deleteError, setDeleteError] = useState<string>();
  const [downloadTarget, setDownloadTarget] = useState<Backup>();
  const [restoreTarget, setRestoreTarget] = useState<Backup>();

  const rows = visibleBackups(backups.data?.backups ?? []);
  const canCreate = canCreateBackup(role);
  const canDelete = canDeleteBackup(role);
  const canRestore = canRestoreBackup(role);

  function openCreate(): void {
    create.reset();
    setDescription('');
    setCreateError(undefined);
    setCreateOpen(true);
  }

  function closeCreate(): void {
    setCreateOpen(false);
    setDescription('');
    setCreateError(undefined);
  }

  function submitCreate(): void {
    setCreateError(undefined);
    create.mutate(description, {
      onSuccess: () => {
        closeCreate();
        toastManager.add({ type: 'success', title: 'Backup started' });
      },
      onError: (error) => {
        setCreateError(createBackupErrorMessage(error));
      },
    });
  }

  const columns: SimpleTableColumn<Backup>[] = [
    {
      id: 'description',
      header: 'Snapshot',
      cell: (row) => (
        <div className="flex min-w-0 flex-col gap-0.5">
          <span className="truncate font-medium">
            {row.description?.trim() || 'Untitled snapshot'}
          </span>
          <span className="text-muted-foreground font-mono text-xs">
            {row.id}
          </span>
        </div>
      ),
    },
    {
      id: 'created',
      header: 'Created',
      cell: (row) => (
        <RelativeTime value={row.backup_at} className="text-cell" />
      ),
    },
    {
      id: 'size',
      header: 'Size',
      align: 'right',
      cell: (row) => (
        <span className="tabular-nums">{backupDisplaySize(row)}</span>
      ),
    },
    {
      id: 'status',
      header: 'Status',
      cell: (row) => expiryPill(row),
    },
    {
      id: 'actions',
      header: <span className="sr-only">Actions</span>,
      align: 'right',
      cell: (row) => (
        <DropdownMenu>
          <DropdownMenuTrigger
            aria-label={`Actions for ${snapshotLabel(row)}`}
            className="hover:bg-muted inline-flex size-7 items-center justify-center rounded-md"
          >
            <Ellipsis className="size-4" aria-hidden="true" />
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end">
            <DropdownMenuItem
              onClick={() => {
                setDownloadTarget(row);
              }}
            >
              <Download aria-hidden="true" />
              Download
            </DropdownMenuItem>
            {canRestore ? (
              <DropdownMenuItem
                onClick={() => {
                  setRestoreTarget(row);
                }}
              >
                <RotateCcw aria-hidden="true" />
                Restore
              </DropdownMenuItem>
            ) : null}
            {canDelete ? (
              <>
                <DropdownMenuSeparator />
                <DropdownMenuItem
                  className="text-destructive"
                  onClick={() => {
                    remove.reset();
                    setDeleteError(undefined);
                    setDeleteTarget(row);
                  }}
                >
                  <Trash2 aria-hidden="true" />
                  Delete
                </DropdownMenuItem>
              </>
            ) : null}
          </DropdownMenuContent>
        </DropdownMenu>
      ),
    },
  ];

  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-muted-foreground text-sm">
          Point-in-time snapshots of this app&apos;s data and files.
        </p>
        <Button
          type="button"
          disabled={!canCreate || hasActiveJob}
          title={
            canCreate
              ? hasActiveJob
                ? 'A backup is already in progress for this app.'
                : undefined
              : 'Only app collaborators, administrators and owners can create backups.'
          }
          onClick={openCreate}
        >
          <Plus aria-hidden="true" />
          Create backup
        </Button>
      </div>

      {hasActiveJob ? (
        <p className="text-muted-foreground text-xs" role="status">
          A backup is running. Its progress is on the Jobs tab.
        </p>
      ) : null}

      {backups.isError && backups.data === undefined ? (
        <ErrorState
          thing="backups"
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
          }}
        />
      ) : backups.isPending ? (
        <TableSkeleton columns={5} />
      ) : rows.length === 0 ? (
        <EmptyState
          icon={DatabaseBackup}
          title="No backups yet"
          description="Create a snapshot before a migration, or set up a schedule so recovery points are taken for you."
          action={
            canCreate ? (
              <Button
                type="button"
                disabled={hasActiveJob}
                onClick={openCreate}
              >
                <Plus aria-hidden="true" />
                Create backup
              </Button>
            ) : undefined
          }
        />
      ) : (
        <SimpleTable
          caption="Backups"
          columns={columns}
          rows={rows}
          getRowId={(row) => row.id}
          loading={backups.isPending}
          empty="No backups yet"
        />
      )}

      <Dialog
        open={createOpen}
        onOpenChange={(open) => {
          if (!open) closeCreate();
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Create backup</DialogTitle>
            <DialogDescription>
              Take a point-in-time snapshot of this app&apos;s data and files.
              Large apps can take a while.
            </DialogDescription>
          </DialogHeader>
          <div className="mt-4 flex flex-col gap-3">
            <label htmlFor="backup-description" className="text-sm font-medium">
              Description (optional)
            </label>
            <Input
              id="backup-description"
              value={description}
              autoComplete="off"
              placeholder="Before migration"
              onChange={(event) => {
                setDescription(event.target.value);
              }}
            />
            {createError === undefined ? null : (
              <p role="alert" className="text-destructive text-sm">
                {createError}
              </p>
            )}
          </div>
          <DialogFooter>
            <Button type="button" variant="secondary" onClick={closeCreate}>
              Cancel
            </Button>
            <Button
              type="button"
              disabled={create.isPending}
              onClick={submitCreate}
            >
              {create.isPending ? 'Starting…' : 'Create backup'}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <ConfirmDialog
        open={deleteTarget !== undefined}
        onOpenChange={(open) => {
          if (!open) setDeleteTarget(undefined);
        }}
        title="Delete backup"
        description={
          <span className="flex flex-col gap-2">
            <span>
              This permanently removes{' '}
              {deleteTarget === undefined
                ? 'the snapshot'
                : snapshotLabel(deleteTarget)}
              . Download it first if you may need it later.
            </span>
            {deleteError === undefined ? null : (
              <span role="alert" className="text-destructive">
                {deleteError}
              </span>
            )}
          </span>
        }
        confirmLabel="Delete backup"
        confirming={remove.isPending}
        onConfirm={() => {
          if (deleteTarget === undefined) return;
          remove.mutate(deleteTarget.id, {
            onSuccess: () => {
              setDeleteTarget(undefined);
              toastManager.add({ type: 'success', title: 'Backup deleted' });
            },
            onError: (error) => {
              setDeleteError(
                error instanceof Error
                  ? error.message
                  : 'Could not delete the backup.',
              );
            },
          });
        }}
      />

      {downloadTarget === undefined ? null : (
        <BackupDownloadSheet
          appId={app.id}
          backup={downloadTarget}
          onClose={() => {
            setDownloadTarget(undefined);
          }}
        />
      )}

      {restoreTarget === undefined ? null : (
        <RestoreBackupSheet
          appId={app.id}
          appTitle={app.title}
          backup={restoreTarget}
          onClose={() => {
            setRestoreTarget(undefined);
          }}
        />
      )}
    </div>
  );
}
