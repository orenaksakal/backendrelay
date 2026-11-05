import { apiFetch } from '@/lib/api/client';
import { storageEndpoints } from '@/lib/api/endpoints';

/**
 * C.9 upload/delete. Both go through the dashboard API with the operator's
 * session (not the app admin token); the server enforces the collaborator role
 * and rewrites the file catalog.
 */

export interface UploadStorageFileOptions {
  appId: string;
  path: string;
  file: Blob;
  signal?: AbortSignal;
}

export async function uploadStorageFile({
  appId,
  path,
  file,
  signal,
}: UploadStorageFileOptions): Promise<void> {
  await apiFetch(storageEndpoints.upload, {
    params: { appId, path },
    body: file,
    signal,
  });
}

export interface DeleteStorageFilesOptions {
  appId: string;
  filenames: ReadonlyArray<string>;
}

export async function deleteStorageFiles({
  appId,
  filenames,
}: DeleteStorageFilesOptions): Promise<void> {
  await apiFetch(storageEndpoints.deleteFiles, {
    params: { appId },
    body: { filenames: [...filenames] },
  });
}
