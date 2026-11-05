import { useEffect, useMemo, useState } from 'react';
import { CopyButton } from '@/components/data/copy-button';
import {
  SimpleTable,
  type SimpleTableColumn,
} from '@/components/data/simple-table';
import { TableSkeleton } from '@/components/feedback/skeletons';
import { Button } from '@/components/ui/button';
import {
  Sheet,
  SheetBody,
  SheetContent,
  SheetDescription,
  SheetFooter,
  SheetHeader,
  SheetTitle,
} from '@/components/ui/sheet';
import { apiBaseUrlFor } from '@/lib/api/client';
import type { Backup } from '@/lib/api/endpoints';
import {
  createBackupsManager,
  directoryPickerAvailable,
  downloadAllFiles,
  downloadFiles,
  loadBackupManifest,
  pickDownloadDirectory,
  type BackupDirectoryHandle,
  type BackupManifest,
  type DownloadFile,
} from '@/lib/backups/download';
import {
  backupDisplaySize,
  backupTime,
  cliDownloadCommand,
} from '@/lib/backups/model';
import { formatBytes } from '@/lib/format';
import { getRuntimeConfig } from '@/lib/runtime-config';
import { toastManager } from '@/lib/toast';
import { BackupProgress } from './backup-progress';

export interface BackupDownloadSheetProps {
  appId: string;
  backup: Backup;
  onClose: () => void;
}

interface DownloadState {
  done: number;
  total: number;
}

/**
 * C.15 download: lists the snapshot's entity files and storage blobs, then
 * saves them through the File System Access API or as sequential downloads.
 */
export function BackupDownloadSheet({
  appId,
  backup,
  onClose,
}: BackupDownloadSheetProps) {
  const [manifest, setManifest] = useState<BackupManifest>();
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string>();
  const [downloading, setDownloading] = useState<DownloadState>();
  const [downloadError, setDownloadError] = useState<string>();
  const [directory, setDirectory] = useState<BackupDirectoryHandle | null>();

  useEffect(() => {
    const controller = new AbortController();
    let active = true;
    // eslint-disable-next-line react-hooks/set-state-in-effect -- the download starts when the sheet opens
    setLoading(true);
    setLoadError(undefined);
    let manager: ReturnType<typeof createBackupsManager>;
    try {
      manager = createBackupsManager(appId);
    } catch (error) {
      setLoadError(
        error instanceof Error ? error.message : 'Could not list the files.',
      );
      setLoading(false);
      return () => {
        active = false;
      };
    }
    loadBackupManifest(manager, backup.id, controller.signal)
      .then((result) => {
        if (active) {
          setManifest(result);
          setLoading(false);
        }
      })
      .catch((error: unknown) => {
        if (!active || controller.signal.aborted) return;
        setLoadError(
          error instanceof Error ? error.message : 'Could not list the files.',
        );
        setLoading(false);
      });
    return () => {
      active = false;
      controller.abort();
    };
  }, [appId, backup.id]);

  const files = useMemo(
    () => (manifest === undefined ? [] : downloadFiles(manifest)),
    [manifest],
  );

  const config = getRuntimeConfig();
  const cliCommand = cliDownloadCommand(
    config === undefined ? '' : apiBaseUrlFor('/dash', config),
    appId,
    backup.id,
  );

  async function downloadAll(): Promise<void> {
    if (manifest === undefined || downloading !== undefined) return;
    setDownloadError(undefined);
    setDownloading({ done: 0, total: files.length });
    try {
      let target: BackupDirectoryHandle | null = null;
      if (directoryPickerAvailable()) {
        // Ask once for a folder so every file lands there; dismissing the
        // picker aborts the whole action.
        target = await pickDownloadDirectory();
        setDirectory(target);
      }
      await downloadAllFiles({
        manager: createBackupsManager(appId),
        backupId: backup.id,
        manifest,
        files,
        directory: target,
        onProgress: (done, total) => {
          setDownloading({ done, total });
        },
      });
      setDownloading(undefined);
      toastManager.add({
        type: 'success',
        title: target ? 'Backup saved' : 'Backup downloads started',
      });
    } catch (error) {
      if ((error as { name?: string }).name === 'AbortError') {
        setDownloading(undefined);
        return;
      }
      setDownloading(undefined);
      setDownloadError(
        error instanceof Error
          ? error.message
          : 'Could not download the backup.',
      );
    }
  }

  const columns: SimpleTableColumn<DownloadFile>[] = [
    {
      id: 'name',
      header: 'File',
      cell: (row) => (
        <div className="flex min-w-0 flex-col gap-0.5">
          <span className="text-cell truncate font-mono">{row.name}</span>
          {row.path !== null && row.path !== row.name ? (
            <span className="text-muted-foreground truncate text-xs">
              {row.path}
            </span>
          ) : null}
        </div>
      ),
    },
    {
      id: 'kind',
      header: 'Contents',
      cell: (row) => (row.kind === 'entity' ? 'Entities' : 'Storage file'),
    },
    {
      id: 'size',
      header: 'Size',
      align: 'right',
      cell: (row) =>
        row.size === null ? (
          '—'
        ) : (
          <span className="tabular-nums">{formatBytes(row.size)}</span>
        ),
    },
  ];

  return (
    <Sheet
      open
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
    >
      <SheetContent size="md">
        <SheetHeader>
          <SheetTitle>Download backup</SheetTitle>
          <SheetDescription>
            The snapshot from {backupTime(backup.backup_at)} ·{' '}
            {backupDisplaySize(backup)} uncompressed.
          </SheetDescription>
        </SheetHeader>

        <SheetBody className="flex flex-col gap-4">
          {loadError === undefined ? null : (
            <p role="alert" className="text-destructive text-sm">
              {loadError}
            </p>
          )}
          {downloadError === undefined ? null : (
            <p role="alert" className="text-destructive text-sm">
              {downloadError}
            </p>
          )}

          {loading ? (
            <TableSkeleton columns={3} />
          ) : files.length === 0 ? (
            <p className="text-muted-foreground text-sm">
              This snapshot has no files.
            </p>
          ) : (
            <SimpleTable
              caption="Backup files"
              columns={columns}
              rows={files}
              getRowId={(row) => `${row.kind}:${row.key}`}
              empty="This snapshot has no files."
            />
          )}

          {downloading === undefined ? null : (
            <div className="flex flex-col gap-1">
              <BackupProgress
                value={
                  downloading.total === 0
                    ? 0
                    : Math.round((downloading.done / downloading.total) * 100)
                }
                label="Download progress"
              />
              <p
                className="text-muted-foreground text-xs tabular-nums"
                role="status"
              >
                {downloading.done} of {downloading.total} files
                {directory ? ' saved to the chosen folder' : ''}
              </p>
            </div>
          )}
        </SheetBody>

        <SheetFooter className="justify-between">
          <CopyButton
            value={cliCommand}
            label="Copy CLI command"
            aria-label="Copy CLI command"
          />
          <div className="flex items-center gap-2">
            <Button type="button" variant="secondary" onClick={onClose}>
              Close
            </Button>
            <Button
              type="button"
              disabled={
                manifest === undefined ||
                files.length === 0 ||
                downloading !== undefined
              }
              onClick={() => {
                void downloadAll();
              }}
            >
              {downloading === undefined ? 'Download all' : 'Downloading…'}
            </Button>
          </div>
        </SheetFooter>
      </SheetContent>
    </Sheet>
  );
}
