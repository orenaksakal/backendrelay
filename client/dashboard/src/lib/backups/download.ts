import {
  BackupsManager,
  type AppBackupFile,
  type AppBackupStorageFile,
} from '@instantdb/platform';
import { apiBaseUrlFor, getApiToken } from '@/lib/api/client';
import { getRuntimeConfig } from '@/lib/runtime-config';
import { downloadNameForEntityFile, downloadNameForStorageFile } from './model';

/**
 * C.15 download. The platform `BackupsManager` discovers the archive's entity
 * files and storage blobs; the browser writes them through the File System
 * Access API when it is available, and falls back to sequential downloads.
 */

export interface BackupManifest {
  files: AppBackupFile[];
  storageFiles: AppBackupStorageFile[];
}

export interface DownloadFile {
  kind: 'entity' | 'storage';
  /** Entity archive name, or the storage blob's location ID. */
  key: string;
  name: string;
  size: number | null;
  path: string | null;
}

/** A minimal view of the directory handle the picker returns. */
export interface BackupDirectoryHandle {
  getFileHandle(
    name: string,
    options: { create: boolean },
  ): Promise<{
    createWritable(): Promise<{
      write(data: Blob): Promise<void>;
      close(): Promise<void>;
    }>;
  }>;
}

interface DirectoryPickerWindow {
  showDirectoryPicker?: () => Promise<BackupDirectoryHandle>;
}

export function createBackupsManager(appId: string): BackupsManager {
  const config = getRuntimeConfig();
  if (config === undefined) {
    throw new Error('The dashboard configuration has not loaded yet.');
  }
  return new BackupsManager({
    appId,
    apiURI: apiBaseUrlFor('/dash', config),
    withAuth: (operation) => {
      const token = getApiToken();
      if (token === null || token === '') {
        throw new Error('Sign in again to download backups.');
      }
      return operation(token);
    },
  });
}

export async function loadBackupManifest(
  manager: BackupsManager,
  backupId: string,
  signal?: AbortSignal,
): Promise<BackupManifest> {
  const files = await manager.listFiles(backupId, { signal });
  const storageFiles: AppBackupStorageFile[] = [];
  for await (const file of manager.streamStorageFiles(backupId, { signal })) {
    storageFiles.push(file);
  }
  return { files, storageFiles };
}

/** Flattens the manifest into the rows the download sheet lists. */
export function downloadFiles(manifest: BackupManifest): DownloadFile[] {
  return [
    ...manifest.files.map((file) => ({
      kind: 'entity' as const,
      key: file.name,
      name: downloadNameForEntityFile(file.name),
      size: file.size,
      path: null,
    })),
    ...manifest.storageFiles.map((file) => ({
      kind: 'storage' as const,
      key: file.locationId,
      name: downloadNameForStorageFile(file.path, file.locationId),
      size: null,
      path: file.path,
    })),
  ];
}

export function directoryPickerAvailable(
  browserWindow: unknown = globalThis.window,
): boolean {
  return (
    typeof browserWindow === 'object' &&
    browserWindow !== null &&
    typeof (browserWindow as DirectoryPickerWindow).showDirectoryPicker ===
      'function'
  );
}

export async function pickDownloadDirectory(
  browserWindow: unknown = globalThis.window,
): Promise<BackupDirectoryHandle | null> {
  const picker = (browserWindow as DirectoryPickerWindow | undefined)
    ?.showDirectoryPicker;
  if (typeof picker !== 'function') return null;
  return picker.call(browserWindow);
}

export async function fileUrlFor(
  manager: BackupsManager,
  backupId: string,
  file: DownloadFile,
  manifest: BackupManifest,
): Promise<string> {
  if (file.kind === 'entity') {
    return manager.getFileUrl(backupId, file.key);
  }
  const storage = manifest.storageFiles.find(
    (candidate) => candidate.locationId === file.key,
  );
  if (storage === undefined) {
    throw new Error(`The backup has no storage file ${file.key}.`);
  }
  return storage.url;
}

async function writeToDirectory(
  directory: BackupDirectoryHandle,
  url: string,
  name: string,
): Promise<void> {
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(`HTTP ${String(response.status)} for ${name}.`);
  }
  const handle = await directory.getFileHandle(name, { create: true });
  const writable = await handle.createWritable();
  await writable.write(await response.blob());
  await writable.close();
}

async function saveWithBrowser(
  url: string,
  name: string,
  signal?: AbortSignal,
): Promise<void> {
  // Presigned object URLs are cross-origin and served without a
  // Content-Disposition, so the browser ignores `download` on them. Fetching
  // first lets a blob URL save the file under its intended name.
  const response = await fetch(url, { signal });
  if (!response.ok) {
    throw new Error(`HTTP ${String(response.status)} for ${name}.`);
  }
  const objectUrl = URL.createObjectURL(await response.blob());
  try {
    const anchor = document.createElement('a');
    anchor.href = objectUrl;
    anchor.download = name;
    anchor.rel = 'noopener';
    document.body.append(anchor);
    anchor.click();
    anchor.remove();
  } finally {
    URL.revokeObjectURL(objectUrl);
  }
}

export interface DownloadAllOptions {
  manager: BackupsManager;
  backupId: string;
  manifest: BackupManifest;
  files: readonly DownloadFile[];
  directory?: BackupDirectoryHandle | null;
  onProgress?: (done: number, total: number) => void;
  signal?: AbortSignal;
}

/**
 * Downloads every file in order. With a directory handle the files stream
 * straight to disk; otherwise each file is saved by the browser in turn.
 */
export async function downloadAllFiles({
  manager,
  backupId,
  manifest,
  files,
  directory,
  onProgress,
  signal,
}: DownloadAllOptions): Promise<void> {
  onProgress?.(0, files.length);
  for (const [index, file] of files.entries()) {
    signal?.throwIfAborted();
    const url = await fileUrlFor(manager, backupId, file, manifest);
    if (directory) {
      await writeToDirectory(directory, url, file.name);
    } else {
      await saveWithBrowser(url, file.name, signal);
    }
    onProgress?.(index + 1, files.length);
  }
}
