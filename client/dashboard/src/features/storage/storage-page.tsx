import {
  File as FileIcon,
  Folder,
  MoreHorizontal,
  Pause,
  Play,
  RefreshCw,
  Upload,
} from 'lucide-react';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { AppDataGate } from '@/components/data/app-data-gate';
import { Bytes } from '@/components/data/formatters';
import { RelativeTime } from '@/components/data/relative-time';
import { SearchInput } from '@/components/data/search-input';
import {
  SimpleTable,
  type SimpleTableColumn,
} from '@/components/data/simple-table';
import { ConfirmDialog } from '@/components/feedback/confirm-dialog';
import { EmptyState } from '@/components/feedback/empty-state';
import { ErrorState } from '@/components/feedback/error-state';
import { TableSkeleton } from '@/components/feedback/skeletons';
import { TypedConfirmDialog } from '@/components/feedback/typed-confirm-dialog';
import { PageHeader } from '@/components/layout/page-header';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import type { App, EffectiveRole } from '@/lib/api/endpoints/schemas';
import { copyText } from '@/lib/clipboard';
import { formatBytes, formatCount } from '@/lib/format';
import { peekAppClient } from '@/lib/instant';
import { PAGE_ACTION_EVENT } from '@/lib/shortcuts';
import { deleteStorageFiles, uploadStorageFile } from '@/lib/storage/api';
import { useFiles } from '@/lib/storage/rows';
import {
  canWriteStorage,
  conflictingUploadPaths,
  FILES_PAGE_SIZE,
  fileContentType,
  fileDeletionPaths,
  fileName,
  fileSize,
  fileUploadedAt,
  fileUrl,
  folderEntries,
  joinStoragePath,
  normalizePrefix,
  prefixSegments,
  visibleFiles,
  type FileRow,
  type StorageFolder,
} from '@/lib/storage/storage';
import { storageSearchFor, type StorageSearch } from '@/lib/storage/url-state';
import { showErrorToast, toastManager } from '@/lib/toast';
import { FileSheet } from './file-sheet';
import { GoToPathDialog } from './go-to-path-dialog';
import { UploadPanel, type UploadTaskView } from './upload-panel';

function signalAborted(signal: AbortSignal): boolean {
  return signal.aborted;
}

export interface StoragePageProps {
  app: App;
  role: EffectiveRole;
  search: StorageSearch;
  /** Route-owned URL write; the folder, search and open sheet live there. */
  onSearchChange: (search: StorageSearch) => void;
}

/** C.9 Storage: the role gate and page header live outside the data gate. */
export function StoragePage({
  app,
  role,
  search,
  onSearchChange,
}: StoragePageProps) {
  return (
    <div className="flex flex-col gap-4">
      <PageHeader
        title="Storage"
        description="Files uploaded to this app, grouped by the folders in their paths."
      />
      <AppDataGate
        app={app}
        role={role}
        thing="files"
        forbiddenMessage="The file list needs admin access to this app."
      >
        <StorageWorkspace
          app={app}
          role={role}
          search={search}
          onSearchChange={onSearchChange}
        />
      </AppDataGate>
    </div>
  );
}

interface StorageWorkspaceProps {
  app: App;
  role: EffectiveRole;
  search: StorageSearch;
  onSearchChange: StoragePageProps['onSearchChange'];
}

type StorageRow =
  | { kind: 'folder'; id: string; folder: StorageFolder }
  | { kind: 'file'; id: string; file: FileRow };

interface UploadTask extends UploadTaskView {
  file: File;
  path: string;
}

interface UploadPrompt {
  files: File[];
  conflicts: string[];
}

function uploadFailureMessage(error: unknown): string {
  return error instanceof Error && error.message.trim() !== ''
    ? error.message
    : 'Upload failed.';
}

function deleteDescriptionFor(rows: FileRow[] | null): string {
  if (rows === null) return '';
  const first = rows[0];
  if (rows.length === 1 && first !== undefined) {
    return `${fileName(first)} is removed from this app. Links to it stop working.`;
  }
  if (rows.length === 1) {
    return 'The file is removed from this app. Links to it stop working.';
  }
  return `${formatCount(rows.length)} files are removed from this app. Links to them stop working.`;
}

function StorageWorkspace({
  app,
  role,
  search,
  onSearchChange,
}: StorageWorkspaceProps) {
  const client = peekAppClient(app.id);
  const prefix = normalizePrefix(search.prefix);
  const canWrite = canWriteStorage(role);

  const files = useFiles({
    client: client ?? null,
    prefix,
    text: search.q ?? '',
    limit: FILES_PAGE_SIZE,
  });
  const folders = useMemo(
    () => folderEntries(files.files, prefix),
    [files.files, prefix],
  );
  const shown = useMemo(
    () => visibleFiles(files.files, prefix, search.q ?? ''),
    [files.files, prefix, search.q],
  );
  const totalBytes = useMemo(
    () => shown.reduce((sum, file) => sum + (fileSize(file) ?? 0), 0),
    [shown],
  );

  const toolbarRef = useRef<HTMLDivElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const controllers = useRef(new Map<string, AbortController>());
  const taskCounter = useRef(0);

  const [selectedIds, setSelectedIds] = useState<string[]>([]);
  const [goToPathOpen, setGoToPathOpen] = useState(false);
  const [deleteTargets, setDeleteTargets] = useState<FileRow[] | null>(null);
  const [confirmingDelete, setConfirmingDelete] = useState(false);
  const [uploadPrompt, setUploadPrompt] = useState<UploadPrompt | null>(null);
  const [tasks, setTasks] = useState<UploadTask[]>([]);
  const [announcement, setAnnouncement] = useState('');
  const [dragging, setDragging] = useState(false);

  // A new folder or search starts a fresh selection.
  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect -- a new folder or search starts a fresh selection
    setSelectedIds([]);
  }, [prefix, search.q]);

  useEffect(() => {
    const current = controllers.current;
    return () => {
      for (const controller of current.values()) controller.abort();
    };
  }, []);

  const navigateTo = useCallback(
    (nextPrefix: string) => {
      onSearchChange(storageSearchFor({ prefix: nextPrefix }));
    },
    [onSearchChange],
  );

  const openFile = useCallback(
    (fileId: string) => {
      onSearchChange(storageSearchFor({ prefix, q: search.q, file: fileId }));
    },
    [onSearchChange, prefix, search.q],
  );

  const closeFile = useCallback(() => {
    onSearchChange(storageSearchFor({ prefix, q: search.q }));
  }, [onSearchChange, prefix, search.q]);

  const updateQuery = useCallback(
    (value: string) => {
      onSearchChange(storageSearchFor({ prefix, q: value, file: search.file }));
    },
    [onSearchChange, prefix, search.file],
  );

  function updateTask(id: string, patch: Partial<UploadTaskView>): void {
    setTasks((current) =>
      current.map((task) => (task.id === id ? { ...task, ...patch } : task)),
    );
  }

  async function startUploads(pending: File[]): Promise<void> {
    if (pending.length === 0) return;
    const queued: UploadTask[] = pending.map((file) => {
      taskCounter.current += 1;
      return {
        id: `upload-${String(taskCounter.current)}`,
        name: file.name,
        path: joinStoragePath(prefix, file.name),
        file,
        status: 'queued',
        progress: 0,
      };
    });
    for (const task of queued) {
      controllers.current.set(task.id, new AbortController());
    }
    setTasks((current) => [
      ...current.filter(
        (task) => task.status === 'queued' || task.status === 'uploading',
      ),
      ...queued,
    ]);
    setAnnouncement(
      queued.length === 1
        ? `Uploading ${queued[0]?.name ?? 'file'}…`
        : `Uploading ${String(queued.length)} files to /${prefix}`,
    );

    let uploaded = 0;
    for (const task of queued) {
      const controller = controllers.current.get(task.id);
      if (controller === undefined || controller.signal.aborted) {
        updateTask(task.id, { status: 'cancelled' });
        continue;
      }
      updateTask(task.id, { status: 'uploading' });
      try {
        await uploadStorageFile({
          appId: app.id,
          path: task.path,
          file: task.file,
          signal: controller.signal,
        });
        uploaded += 1;
        updateTask(task.id, { status: 'done', progress: 100 });
        setAnnouncement(`${task.name} uploaded`);
      } catch (failure) {
        if (signalAborted(controller.signal)) {
          updateTask(task.id, { status: 'cancelled' });
          setAnnouncement(`${task.name} upload cancelled`);
        } else {
          updateTask(task.id, {
            status: 'failed',
            error: uploadFailureMessage(failure),
          });
          setAnnouncement(`${task.name} upload failed`);
          showErrorToast(failure);
        }
      } finally {
        controllers.current.delete(task.id);
      }
    }

    if (uploaded > 0) {
      toastManager.add({
        type: 'success',
        title:
          uploaded === 1
            ? '1 file uploaded'
            : `${formatCount(uploaded)} files uploaded`,
      });
    }
  }

  function requestUploads(list: File[]): void {
    if (!canWrite || list.length === 0) return;
    const targets = list.map((file) => joinStoragePath(prefix, file.name));
    const conflicts = conflictingUploadPaths(files.files, targets);
    if (conflicts.length > 0) {
      setUploadPrompt({ files: list, conflicts });
      return;
    }
    void startUploads(list);
  }

  function confirmReplace(): void {
    if (uploadPrompt === null) return;
    const pending = uploadPrompt.files;
    setUploadPrompt(null);
    void startUploads(pending);
  }

  function cancelUpload(id: string): void {
    const task = tasks.find((candidate) => candidate.id === id);
    updateTask(id, { status: 'cancelled' });
    controllers.current.get(id)?.abort();
    if (task !== undefined) setAnnouncement(`${task.name} upload cancelled`);
  }

  function dismissUploads(): void {
    setTasks([]);
    setAnnouncement('');
  }

  function openUploadPicker(): void {
    if (!canWrite) return;
    fileInputRef.current?.click();
  }

  function requestDelete(rows: FileRow[]): void {
    if (!canWrite || rows.length === 0) return;
    setDeleteTargets(rows);
  }

  async function confirmDelete(): Promise<void> {
    if (deleteTargets === null) return;
    setConfirmingDelete(true);
    try {
      const filenames = fileDeletionPaths(deleteTargets);
      await deleteStorageFiles({ appId: app.id, filenames });
      toastManager.add({
        type: 'success',
        title:
          deleteTargets.length === 1
            ? '1 file deleted'
            : `${formatCount(deleteTargets.length)} files deleted`,
      });
      if (
        search.file !== undefined &&
        deleteTargets.some((file) => file.id === search.file)
      ) {
        closeFile();
      }
      const deleted = new Set(deleteTargets.map((file) => file.id));
      setSelectedIds((current) => current.filter((id) => !deleted.has(id)));
      setDeleteTargets(null);
    } catch (failure) {
      showErrorToast(failure);
    } finally {
      setConfirmingDelete(false);
    }
  }

  // `/` focuses the search box and Shift+R refreshes (B.7).
  useEffect(() => {
    const listener = (event: Event) => {
      const detail = (event as CustomEvent<{ action?: string } | undefined>)
        .detail;
      if (detail?.action === 'search') {
        const input = toolbarRef.current?.querySelector<HTMLInputElement>(
          'input[role="searchbox"]',
        );
        if (input === null || input === undefined) return;
        event.preventDefault();
        input.focus();
        return;
      }
      if (detail?.action === 'refresh') {
        event.preventDefault();
        files.refresh();
      }
    };
    window.addEventListener(PAGE_ACTION_EVENT, listener);
    return () => {
      window.removeEventListener(PAGE_ACTION_EVENT, listener);
    };
  }, [files.refresh]);

  if (client === undefined) return null;

  const fileIds = shown.map((file) => file.id);
  const selectedVisible = fileIds.filter((id) => selectedIds.includes(id));
  const allSelected =
    fileIds.length > 0 && selectedVisible.length === fileIds.length;
  const someSelected = selectedVisible.length > 0 && !allSelected;
  const selectedRows = shown.filter((file) => selectedIds.includes(file.id));
  const targetRow = files.files.find((file) => file.id === search.file) ?? null;
  const segments = prefixSegments(prefix);
  const folderCount = folders.length;
  const fileCount = shown.length;
  const countLabel =
    fileCount === 1 ? '1 file' : `${formatCount(fileCount)} files`;

  const rows: StorageRow[] = [
    ...folders.map(
      (folder): StorageRow => ({
        kind: 'folder',
        id: `folder:${folder.path}`,
        folder,
      }),
    ),
    ...shown.map(
      (file): StorageRow => ({ kind: 'file', id: `file:${file.id}`, file }),
    ),
  ];

  const columns: SimpleTableColumn<StorageRow>[] = [];
  if (canWrite) {
    columns.push({
      id: 'select',
      header: (
        <Checkbox
          aria-label="Select all files"
          checked={allSelected}
          indeterminate={someSelected}
          onCheckedChange={(checked) => {
            setSelectedIds(checked ? fileIds : []);
          }}
        />
      ),
      className: 'w-10',
      cell: (row) =>
        row.kind === 'folder' ? null : (
          <Checkbox
            aria-label={`Select ${fileName(row.file)}`}
            checked={selectedIds.includes(row.file.id)}
            onCheckedChange={(checked) => {
              setSelectedIds((current) =>
                checked
                  ? [...new Set([...current, row.file.id])]
                  : current.filter((id) => id !== row.file.id),
              );
            }}
          />
        ),
    });
  }
  columns.push(
    {
      id: 'name',
      header: 'Name',
      cell: (row) =>
        row.kind === 'folder' ? (
          <Button
            type="button"
            variant="link"
            className="h-auto min-h-6 justify-start p-0 text-left font-normal"
            onClick={() => {
              navigateTo(row.folder.path);
            }}
          >
            <Folder aria-hidden="true" className="text-muted-foreground" />
            <span className="text-cell break-all">{row.folder.name}</span>
          </Button>
        ) : (
          <Button
            type="button"
            variant="link"
            className="h-auto min-h-6 justify-start p-0 text-left font-normal"
            onClick={() => {
              openFile(row.file.id);
            }}
          >
            <FileIcon aria-hidden="true" className="text-muted-foreground" />
            <span className="text-cell break-all">{fileName(row.file)}</span>
          </Button>
        ),
    },
    {
      id: 'type',
      header: 'Type',
      cell: (row) =>
        row.kind === 'folder' ? (
          <span className="text-muted-foreground">Folder</span>
        ) : (
          <ResourceValue value={fileContentType(row.file)} />
        ),
    },
    {
      id: 'size',
      header: 'Size',
      align: 'right',
      cell: (row) => {
        if (row.kind === 'folder') {
          return <span className="text-muted-foreground">—</span>;
        }
        const size = fileSize(row.file);
        return size === null ? (
          <span className="text-muted-foreground">—</span>
        ) : (
          <Bytes value={size} />
        );
      },
    },
    {
      id: 'uploaded',
      header: 'Uploaded',
      cell: (row) => {
        if (row.kind === 'folder') {
          return <span className="text-muted-foreground">—</span>;
        }
        const uploaded = fileUploadedAt(row.file);
        return uploaded === null ? (
          <span className="text-muted-foreground">—</span>
        ) : (
          <RelativeTime value={uploaded} className="text-cell" />
        );
      },
    },
    {
      id: 'actions',
      header: <span className="sr-only">Actions</span>,
      className: 'w-10',
      cell: (row) =>
        row.kind === 'folder' ? null : (
          <FileRowMenu
            file={row.file}
            canWrite={canWrite}
            onOpen={() => {
              openFile(row.file.id);
            }}
            onDelete={() => {
              requestDelete([row.file]);
            }}
          />
        ),
    },
  );

  return (
    <div className="flex flex-col gap-3">
      <div ref={toolbarRef} className="flex flex-wrap items-center gap-2">
        <SearchInput
          aria-label="Search files"
          placeholder="Search path…"
          value={search.q ?? ''}
          onValueChange={updateQuery}
          showShortcut
          className="min-w-48 flex-1"
        />
        <Button
          type="button"
          variant="secondary"
          aria-label={
            files.pending ? 'Refresh (new changes available)' : 'Refresh'
          }
          onClick={files.refresh}
        >
          <RefreshCw aria-hidden="true" />
          Refresh
          {files.pending ? (
            <span
              aria-hidden="true"
              className="bg-warning size-1.5 rounded-full"
            />
          ) : null}
        </Button>
        {files.paused ? (
          <Button type="button" variant="secondary" onClick={files.resume}>
            <Play aria-hidden="true" />
            Resume
          </Button>
        ) : (
          <Button
            type="button"
            variant="secondary"
            aria-pressed={true}
            onClick={files.togglePause}
          >
            <Pause aria-hidden="true" />
            Live
          </Button>
        )}
        <Button
          type="button"
          disabled={!canWrite}
          title={canWrite ? undefined : 'This needs the collaborator role.'}
          onClick={openUploadPicker}
        >
          <Upload aria-hidden="true" />
          Upload files
        </Button>
        <input
          ref={fileInputRef}
          type="file"
          multiple
          tabIndex={-1}
          aria-label="Choose files to upload"
          className="sr-only"
          onChange={(event) => {
            const list = Array.from(event.target.files ?? []);
            event.target.value = '';
            requestUploads(list);
          }}
        />
      </div>

      <div className="flex flex-wrap items-end justify-between gap-2">
        <nav
          aria-label="Folder path"
          className="flex flex-wrap items-center gap-1"
        >
          <span className="text-muted-foreground text-xs">Path:</span>
          <Button
            type="button"
            variant="ghost"
            size="sm"
            aria-current={prefix === '' ? 'page' : undefined}
            onClick={() => {
              navigateTo('');
            }}
          >
            /
          </Button>
          {segments.map((segment) => (
            <span key={segment.path} className="flex items-center gap-1">
              <span aria-hidden="true" className="text-muted-foreground">
                /
              </span>
              <Button
                type="button"
                variant="ghost"
                size="sm"
                aria-current={segment.path === prefix ? 'page' : undefined}
                onClick={() => {
                  navigateTo(segment.path);
                }}
              >
                {segment.name}
              </Button>
            </span>
          ))}
          <Button
            type="button"
            variant="link"
            size="sm"
            onClick={() => {
              setGoToPathOpen(true);
            }}
          >
            Go to path…
          </Button>
        </nav>
        <p className="text-muted-foreground text-xs">
          {countLabel}
          {folderCount > 0
            ? ` · ${folderCount === 1 ? '1 folder' : `${formatCount(folderCount)} folders`}`
            : ''}
          {totalBytes > 0 ? ` · ${formatBytes(totalBytes)}` : ''}
        </p>
      </div>

      {files.error !== null ? (
        <ErrorState
          thing="files"
          message={files.error}
          onRetry={files.refresh}
        />
      ) : files.isLoading && files.files.length === 0 ? (
        <TableSkeleton columns={canWrite ? 6 : 5} />
      ) : rows.length === 0 && prefix === '' && search.q === undefined ? (
        <EmptyState
          icon={Folder}
          title="No files yet"
          description="Upload files to add them to this app. Their paths become folders here."
          action={
            canWrite ? (
              <Button type="button" onClick={openUploadPicker}>
                <Upload aria-hidden="true" />
                Upload files
              </Button>
            ) : undefined
          }
        />
      ) : (
        // eslint-disable-next-line jsx-a11y/no-noninteractive-element-interactions -- drag and drop supplements the upload button
        <div
          role="region"
          aria-label="Storage files"
          className="relative flex flex-col gap-2"
          onDragOver={(event) => {
            if (!canWrite) return;
            event.preventDefault();
            setDragging(true);
          }}
          onDragLeave={(event) => {
            const related = event.relatedTarget;
            if (
              related instanceof Node &&
              event.currentTarget.contains(related)
            ) {
              return;
            }
            setDragging(false);
          }}
          onDrop={(event) => {
            if (!canWrite) return;
            event.preventDefault();
            setDragging(false);
            requestUploads(Array.from(event.dataTransfer.files));
          }}
        >
          <SimpleTable
            caption="Files"
            columns={columns}
            rows={rows}
            getRowId={(row) => row.id}
            onRowActivate={(row) => {
              if (row.kind === 'folder') {
                navigateTo(row.folder.path);
              } else {
                openFile(row.file.id);
              }
            }}
            empty={
              <span className="inline-flex flex-wrap items-center justify-center gap-1">
                No files in this folder.
                {search.q === undefined ? null : (
                  <Button
                    type="button"
                    variant="link"
                    size="sm"
                    onClick={() => {
                      updateQuery('');
                    }}
                  >
                    Clear search
                  </Button>
                )}
              </span>
            }
          />

          {canWrite && selectedRows.length > 0 ? (
            <div className="border-border bg-card sticky bottom-2 z-(--z-sticky) mx-auto flex w-fit flex-wrap items-center gap-3 rounded-lg border px-3 py-1.5 shadow-md">
              <span role="status" className="text-xs">
                {formatCount(selectedRows.length)} selected
              </span>
              <Button
                type="button"
                variant="destructive"
                size="sm"
                onClick={() => {
                  requestDelete(selectedRows);
                }}
              >
                Delete
              </Button>
              <Button
                type="button"
                variant="ghost"
                size="sm"
                onClick={() => {
                  setSelectedIds([]);
                }}
              >
                Clear selection
              </Button>
            </div>
          ) : null}

          {dragging ? (
            <div
              aria-hidden="true"
              className="border-accent bg-background/90 text-accent pointer-events-none absolute inset-0 z-(--z-sticky) flex items-center justify-center rounded-lg border-2 border-dashed text-sm font-medium"
            >
              {`Drop files to upload to ${prefix === '' ? '/' : `/${prefix}`}`}
            </div>
          ) : null}
        </div>
      )}

      {files.volumePaused ? (
        <p role="status" className="text-warning text-xs">
          Live updates paused (high volume). Resume to follow them again.
        </p>
      ) : files.pending && !files.paused ? (
        <p role="status" className="sr-only">
          New changes available
        </p>
      ) : null}

      {search.file === undefined ? null : (
        <FileSheet
          key={search.file}
          client={client}
          fileId={search.file}
          fallback={targetRow}
          onOpenChange={(open) => {
            if (!open) closeFile();
          }}
          onDelete={(file) => {
            requestDelete([file]);
          }}
          canWrite={canWrite}
        />
      )}

      <GoToPathDialog
        open={goToPathOpen}
        onOpenChange={setGoToPathOpen}
        initialPath={prefix === '' ? '' : prefix.replace(/\/$/, '')}
        onGo={navigateTo}
      />

      <UploadPanel
        tasks={tasks}
        announcement={announcement}
        onCancel={cancelUpload}
        onDismiss={dismissUploads}
      />

      <ConfirmDialog
        open={uploadPrompt !== null}
        onOpenChange={(open) => {
          if (!open) setUploadPrompt(null);
        }}
        title="Replace existing file?"
        description={
          uploadPrompt === null ? (
            ''
          ) : uploadPrompt.conflicts.length === 1 ? (
            <>
              <span className="text-cell font-mono break-all">
                {uploadPrompt.conflicts[0]}
              </span>{' '}
              already exists. Replacing it overwrites the stored file.
            </>
          ) : (
            <>
              {formatCount(uploadPrompt.conflicts.length)} files already exist.
              Replacing them overwrites the stored files.
            </>
          )
        }
        confirmLabel="Replace"
        onConfirm={confirmReplace}
      />

      {deleteTargets !== null && deleteTargets.length > 10 ? (
        <TypedConfirmDialog
          open
          onOpenChange={(open) => {
            if (!open) setDeleteTargets(null);
          }}
          title="Delete these files?"
          description={`${formatCount(deleteTargets.length)} selected files are removed from this app.`}
          consequences={[
            'The files are removed from object storage.',
            'Their $files records are deleted and every link stops working.',
          ]}
          confirmText={String(deleteTargets.length)}
          confirmLabel="Delete files"
          confirming={confirmingDelete}
          onConfirm={() => {
            void confirmDelete();
          }}
        />
      ) : (
        <ConfirmDialog
          open={deleteTargets !== null}
          onOpenChange={(open) => {
            if (!open) setDeleteTargets(null);
          }}
          title="Delete this file?"
          description={deleteDescriptionFor(deleteTargets)}
          confirmLabel={
            deleteTargets !== null && deleteTargets.length > 1
              ? 'Delete files'
              : 'Delete file'
          }
          confirming={confirmingDelete}
          onConfirm={() => {
            void confirmDelete();
          }}
        />
      )}
    </div>
  );
}

function ResourceValue({ value }: { value: string | null }) {
  if (value === null) {
    return <span className="text-muted-foreground">—</span>;
  }
  return <span className="text-cell">{value}</span>;
}

interface FileRowMenuProps {
  file: FileRow;
  canWrite: boolean;
  onOpen: () => void;
  onDelete: () => void;
}

function FileRowMenu({ file, canWrite, onOpen, onDelete }: FileRowMenuProps) {
  const url = fileUrl(file);
  const name = fileName(file);
  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        render={
          <Button
            type="button"
            variant="ghost"
            size="sm"
            className="size-7 p-0"
            aria-label={`Actions for ${name}`}
          />
        }
      >
        <MoreHorizontal aria-hidden="true" />
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end">
        <DropdownMenuItem onClick={onOpen}>Open file</DropdownMenuItem>
        <DropdownMenuItem
          disabled={url === null}
          onClick={() => {
            if (url === null) return;
            void copyText(url).then((copied) => {
              toastManager.add({
                type: copied ? 'success' : 'error',
                title: copied
                  ? 'File URL copied'
                  : "Couldn't copy the file URL",
              });
            });
          }}
        >
          Copy URL
        </DropdownMenuItem>
        {url === null ? null : (
          <DropdownMenuItem
            render={<a href={url} download={name} aria-label="Download" />}
          >
            Download
          </DropdownMenuItem>
        )}
        <DropdownMenuSeparator />
        <DropdownMenuItem
          className="text-destructive"
          disabled={!canWrite}
          onClick={onDelete}
        >
          Delete file
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
