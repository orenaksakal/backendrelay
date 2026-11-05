import type { InstaqlWhere } from '@/components/data/filter-serialization';
import type { EffectiveRole } from '@/lib/api/endpoints/schemas';
import { roleRank } from '@/lib/organization/model';

/**
 * C.9 Storage: `$files` entities, the virtual folder tree derived from their
 * paths, and the query/upload/delete rules the screen needs. Folders are never
 * stored; a path like `avatars/2026/cat.png` only appears at `avatars/2026/`
 * because its files do.
 */

export const FILES_NAMESPACE = '$files';

/** C.9 loads one folder at a time, newest first. */
export const FILES_PAGE_SIZE = 100;

/** A `$files` entity. Only `id` is guaranteed; a missing attribute is an em dash. */
export type FileRow = { id: string } & Record<string, unknown>;

export interface StorageFolder {
  name: string;
  /** Canonical prefix including the trailing slash. */
  path: string;
}

export interface StoragePrefixSegment {
  name: string;
  path: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function isUnknownArray(value: unknown): value is unknown[] {
  return Array.isArray(value);
}

export function isFileRow(value: unknown): value is FileRow {
  return isRecord(value) && typeof value['id'] === 'string';
}

/**
 * Canonical folder prefix: no leading slash, no empty or relative segments,
 * and a trailing slash when non-empty (`avatars/2026/`). The empty string is
 * the root.
 */
export function normalizePrefix(value: string | undefined): string {
  if (value === undefined) return '';
  const segments = value
    .split('/')
    .map((segment) => segment.trim())
    .filter((segment) => segment !== '' && segment !== '.' && segment !== '..');
  return segments.length === 0 ? '' : `${segments.join('/')}/`;
}

/** The breadcrumb trail for a prefix: `/ avatars / 2026 /`. */
export function prefixSegments(prefix: string): StoragePrefixSegment[] {
  const names = normalizePrefix(prefix).split('/').filter(Boolean);
  let path = '';
  return names.map((name) => {
    path = `${path}${name}/`;
    return { name, path };
  });
}

/** Appends a file or folder name to a prefix without doubling the slash. */
export function joinStoragePath(prefix: string, name: string): string {
  return `${normalizePrefix(prefix)}${name.replace(/^\/+/, '')}`;
}

export function filePath(file: FileRow): string | null {
  const path = file['path'];
  if (typeof path !== 'string' || path.trim() === '') return null;
  return path;
}

/** The last path segment, or the entity id for a file with no path. */
export function fileName(file: FileRow): string {
  const path = filePath(file);
  if (path === null) return file.id;
  const name = path.split('/').at(-1);
  return name === undefined || name === '' ? file.id : name;
}

export function fileContentType(file: FileRow): string | null {
  const value = file['content-type'];
  if (typeof value !== 'string' || value.trim() === '') return null;
  return value;
}

export function fileSize(file: FileRow): number | null {
  const value = file['size'];
  if (typeof value !== 'number' || !Number.isFinite(value) || value < 0) {
    return null;
  }
  return value;
}

export function fileUrl(file: FileRow): string | null {
  const value = file['url'];
  if (typeof value !== 'string' || value.trim() === '') return null;
  return value;
}

/**
 * The server does not guarantee a created time on `$files`. Read the shapes a
 * catalog may expose and let the column show an em dash when none exists.
 */
export function fileUploadedAt(file: FileRow): string | number | null {
  for (const key of ['serverCreatedAt', 'createdAt', 'created-at']) {
    const value = file[key];
    if (typeof value === 'string' || typeof value === 'number') return value;
  }
  return null;
}

/** The folder a file appears under at `prefix`, or `null` when it is direct. */
export function fileFolder(
  file: FileRow,
  prefix: string,
): StorageFolder | null {
  const path = filePath(file);
  if (path === null) return null;
  const normalized = normalizePrefix(prefix);
  if (!path.startsWith(normalized)) return null;
  const rest = path.slice(normalized.length);
  const slash = rest.indexOf('/');
  if (slash <= 0) return null;
  const name = rest.slice(0, slash);
  return { name, path: `${normalized}${name}/` };
}

/** Distinct folders directly under `prefix`, sorted by name (C.9). */
export function folderEntries(
  files: ReadonlyArray<FileRow>,
  prefix: string,
): StorageFolder[] {
  const normalized = normalizePrefix(prefix);
  const folders = new Map<string, string>();
  for (const file of files) {
    const folder = fileFolder(file, normalized);
    if (folder !== null) folders.set(folder.name, folder.path);
  }
  return [...folders]
    .map(([name, path]) => ({ name, path }))
    .sort((left, right) => left.name.localeCompare(right.name));
}

/** Files that sit directly in `prefix`, not in one of its subfolders. */
export function filesInPrefix(
  files: ReadonlyArray<FileRow>,
  prefix: string,
): FileRow[] {
  const normalized = normalizePrefix(prefix);
  return files.filter((file) => {
    const path = filePath(file);
    if (path === null || !path.startsWith(normalized)) return false;
    const rest = path.slice(normalized.length);
    return rest !== '' && !rest.includes('/');
  });
}

/**
 * Rows for the table. Without a query only the current folder's files show;
 * a search looks through every path below the prefix, so a match in a nested
 * folder is still reachable.
 */
export function visibleFiles(
  files: ReadonlyArray<FileRow>,
  prefix: string,
  text: string,
): FileRow[] {
  const query = text.trim().toLowerCase();
  if (query === '') return filesInPrefix(files, prefix);
  const normalized = normalizePrefix(prefix);
  return files.filter((file) => {
    const path = filePath(file);
    return (
      path !== null &&
      path.startsWith(normalized) &&
      path.toLowerCase().includes(query)
    );
  });
}

export function buildFilesWhere(prefix: string): InstaqlWhere {
  const normalized = normalizePrefix(prefix);
  return normalized === '' ? {} : { path: { $like: `${normalized}%` } };
}

/** `{ $files: { $: { where, limit, offset, order } } }` (C.9). */
export function buildFilesQuery({
  prefix,
  limit = FILES_PAGE_SIZE,
  offset = 0,
}: {
  prefix: string;
  limit?: number;
  offset?: number;
}): Record<string, unknown> {
  const where = buildFilesWhere(prefix);
  const options: Record<string, unknown> = {
    limit,
    offset,
    order: { serverCreatedAt: 'desc' },
  };
  if (Object.keys(where).length > 0) options['where'] = where;
  return { [FILES_NAMESPACE]: { $: options } };
}

/** One file by id, for a sheet opened from a copied URL. */
export function buildFileQuery(fileId: string): Record<string, unknown> {
  return {
    [FILES_NAMESPACE]: { $: { where: { id: fileId }, limit: 1 } },
  };
}

export interface FilesQueryResult {
  files: FileRow[];
  hasNextPage: boolean;
}

export function parseFilesResult(result: unknown): FilesQueryResult {
  const payload: Record<string, unknown> = isRecord(result) ? result : {};
  const data = isRecord(payload['data'])
    ? payload['data'][FILES_NAMESPACE]
    : undefined;
  const files = isUnknownArray(data)
    ? data.filter((row): row is FileRow => isFileRow(row))
    : [];

  const pageInfo = isRecord(payload['pageInfo'])
    ? payload['pageInfo'][FILES_NAMESPACE]
    : undefined;
  const hasNextPage = isRecord(pageInfo) && pageInfo['hasNextPage'] === true;

  return { files, hasNextPage };
}

/** A query failure message, or `null` for a successful result. */
export function parseFilesError(result: unknown): string | null {
  if (!isRecord(result)) return null;
  const error = result['error'];
  if (!isRecord(error)) return null;
  const message = error['message'];
  return typeof message === 'string' && message !== ''
    ? message
    : 'The query failed.';
}

export type FileKind = 'image' | 'video' | 'audio' | 'pdf' | 'other';

const EXTENSION_KINDS: Record<string, FileKind> = {
  avif: 'image',
  gif: 'image',
  jpeg: 'image',
  jpg: 'image',
  png: 'image',
  svg: 'image',
  webp: 'image',
  m4a: 'audio',
  mp3: 'audio',
  ogg: 'audio',
  wav: 'audio',
  mov: 'video',
  mp4: 'video',
  webm: 'video',
  pdf: 'pdf',
};

/** Preview kind from the content type, falling back to the file extension. */
export function fileKind(contentType: string | null, name: string): FileKind {
  const type = (contentType ?? '').toLowerCase();
  if (type.startsWith('image/')) return 'image';
  if (type.startsWith('video/')) return 'video';
  if (type.startsWith('audio/')) return 'audio';
  if (type === 'application/pdf') return 'pdf';
  const extension = name.includes('.')
    ? name.slice(name.lastIndexOf('.') + 1).toLowerCase()
    : '';
  return EXTENSION_KINDS[extension] ?? 'other';
}

export function fileKindFor(file: FileRow): FileKind {
  return fileKind(fileContentType(file), fileName(file));
}

/** Exact paths for the delete endpoint; a stale row must not delete a guess. */
export function fileDeletionPaths(files: ReadonlyArray<FileRow>): string[] {
  return files.map((file) => {
    const path = filePath(file);
    if (path === null) {
      throw new Error(
        'A selected file is no longer loaded. Refresh and select it again.',
      );
    }
    return path;
  });
}

/** Uploads whose target path already exists in the loaded folder. */
export function conflictingUploadPaths(
  files: ReadonlyArray<FileRow>,
  paths: ReadonlyArray<string>,
): string[] {
  const existing = new Set(
    files.map(filePath).filter((path): path is string => path !== null),
  );
  return [...new Set(paths.filter((path) => existing.has(path)))];
}

/** C.9: upload and delete need the collaborator role, the list needs admin. */
export function canWriteStorage(role: EffectiveRole): boolean {
  return roleRank(role) >= roleRank('collaborator');
}
