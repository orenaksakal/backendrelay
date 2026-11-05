import { describe, expect, it } from 'vitest';
import {
  buildFilesQuery,
  buildFilesWhere,
  buildFileQuery,
  canWriteStorage,
  conflictingUploadPaths,
  fileContentType,
  fileDeletionPaths,
  fileFolder,
  fileKind,
  fileKindFor,
  fileName,
  filePath,
  fileSize,
  fileUploadedAt,
  fileUrl,
  filesInPrefix,
  folderEntries,
  isFileRow,
  joinStoragePath,
  normalizePrefix,
  parseFilesError,
  parseFilesResult,
  prefixSegments,
  visibleFiles,
  type FileRow,
} from './storage';

const files: FileRow[] = [
  {
    id: 'file-avatar',
    path: 'avatar.png',
    size: 12,
    'content-type': 'image/png',
  },
  {
    id: 'file-cat',
    path: 'avatars/2026/cat.png',
    size: 2048,
    'content-type': 'image/png',
  },
  { id: 'file-dog', path: 'avatars/dog.png', size: 4096 },
  { id: 'file-report', path: 'docs/2026/report.pdf', size: 9000 },
];

describe('storage paths', () => {
  it('canonicalises prefixes and keeps the root empty', () => {
    expect(normalizePrefix(undefined)).toBe('');
    expect(normalizePrefix('/')).toBe('');
    expect(normalizePrefix(' /avatars//2026/ ')).toBe('avatars/2026/');
    expect(normalizePrefix('./avatars/../docs')).toBe('avatars/docs/');
  });

  it('builds the breadcrumb trail', () => {
    expect(prefixSegments('avatars/2026/')).toEqual([
      { name: 'avatars', path: 'avatars/' },
      { name: '2026', path: 'avatars/2026/' },
    ]);
    expect(prefixSegments('')).toEqual([]);
  });

  it('joins names without doubling slashes', () => {
    expect(joinStoragePath('', 'avatar.png')).toBe('avatar.png');
    expect(joinStoragePath('avatars/', '/cat.png')).toBe('avatars/cat.png');
    expect(joinStoragePath('avatars', 'cat.png')).toBe('avatars/cat.png');
  });
});

describe('file fields', () => {
  it('reads what the catalog provides and leaves the rest blank', () => {
    expect(filePath({ id: 'file-1', path: 'a/b.png' })).toBe('a/b.png');
    expect(filePath({ id: 'file-1', path: '   ' })).toBeNull();
    expect(fileName({ id: 'file-1', path: 'a/b.png' })).toBe('b.png');
    expect(fileName({ id: 'file-1' })).toBe('file-1');
    expect(fileContentType(files[0] ?? { id: 'file-1' })).toBe('image/png');
    expect(fileSize(files[0] ?? { id: 'file-1' })).toBe(12);
    expect(fileSize({ id: 'file-1', size: -1 })).toBeNull();
    expect(fileUrl({ id: 'file-1', url: 'https://cdn.test/a.png' })).toBe(
      'https://cdn.test/a.png',
    );
    expect(fileUrl({ id: 'file-1' })).toBeNull();
    expect(
      fileUploadedAt({ id: 'file-1', serverCreatedAt: '2026-10-01T00:00:00Z' }),
    ).toBe('2026-10-01T00:00:00Z');
    expect(fileUploadedAt({ id: 'file-1' })).toBeNull();
  });

  it('recognises rows by their id', () => {
    expect(isFileRow({ id: 'file-1' })).toBe(true);
    expect(isFileRow({ path: 'a.png' })).toBe(false);
  });
});

describe('virtual folders', () => {
  it('derives only the folders directly below the prefix', () => {
    expect(folderEntries(files, '')).toEqual([
      { name: 'avatars', path: 'avatars/' },
      { name: 'docs', path: 'docs/' },
    ]);
    expect(folderEntries(files, 'avatars/')).toEqual([
      { name: '2026', path: 'avatars/2026/' },
    ]);
    expect(fileFolder(files[1] ?? { id: 'file-1' }, 'avatars/')).toEqual({
      name: '2026',
      path: 'avatars/2026/',
    });
    expect(fileFolder(files[0] ?? { id: 'file-1' }, '')).toBeNull();
  });

  it('lists direct files separately from nested ones', () => {
    expect(filesInPrefix(files, '').map((file) => file.id)).toEqual([
      'file-avatar',
    ]);
    expect(filesInPrefix(files, 'avatars/').map((file) => file.id)).toEqual([
      'file-dog',
    ]);
  });

  it('searches every path below the prefix', () => {
    expect(visibleFiles(files, '', '').map((file) => file.id)).toEqual([
      'file-avatar',
    ]);
    expect(visibleFiles(files, '', 'CAT').map((file) => file.id)).toEqual([
      'file-cat',
    ]);
    expect(
      visibleFiles(files, 'avatars/', 'dog').map((file) => file.id),
    ).toEqual(['file-dog']);
  });
});

describe('files query', () => {
  it('filters by prefix only when a folder is open', () => {
    expect(buildFilesWhere('')).toEqual({});
    expect(buildFilesWhere('avatars')).toEqual({
      path: { $like: 'avatars/%' },
    });
  });

  it('orders newest first with the C.9 page size', () => {
    expect(buildFilesQuery({ prefix: '' })).toEqual({
      $files: {
        $: { limit: 100, offset: 0, order: { serverCreatedAt: 'desc' } },
      },
    });
    expect(
      buildFilesQuery({ prefix: 'avatars/', limit: 20, offset: 40 }),
    ).toEqual({
      $files: {
        $: {
          where: { path: { $like: 'avatars/%' } },
          limit: 20,
          offset: 40,
          order: { serverCreatedAt: 'desc' },
        },
      },
    });
    expect(buildFileQuery('file-1')).toEqual({
      $files: { $: { where: { id: 'file-1' }, limit: 1 } },
    });
  });

  it('reads the page and the next-page flag', () => {
    const result = parseFilesResult({
      data: {
        $files: [{ id: 'file-1', path: 'a.png' }, { path: 'no-id.png' }],
      },
      pageInfo: { $files: { hasNextPage: true } },
    });
    expect(result.files.map((file) => file.id)).toEqual(['file-1']);
    expect(result.hasNextPage).toBe(true);
    expect(parseFilesResult(undefined)).toEqual({
      files: [],
      hasNextPage: false,
    });
  });

  it('surfaces query errors', () => {
    expect(parseFilesError({ error: { message: 'Permission denied' } })).toBe(
      'Permission denied',
    );
    expect(parseFilesError({ error: {} })).toBe('The query failed.');
    expect(parseFilesError({ data: { $files: [] } })).toBeNull();
  });
});

describe('file kinds and writes', () => {
  it('detects previews from the content type or the extension', () => {
    expect(fileKind('image/png', 'a.bin')).toBe('image');
    expect(fileKind('video/mp4', 'a.bin')).toBe('video');
    expect(fileKind('audio/mpeg', 'a.bin')).toBe('audio');
    expect(fileKind('application/pdf', 'a.bin')).toBe('pdf');
    expect(fileKind(null, 'photo.JPEG')).toBe('image');
    expect(fileKind(null, 'manual.PDF')).toBe('pdf');
    expect(fileKind(null, 'archive.zip')).toBe('other');
    expect(fileKindFor(files[0] ?? { id: 'file-1' })).toBe('image');
  });

  it('resolves exact deletion paths and refuses stale rows', () => {
    expect(fileDeletionPaths([files[0] ?? { id: 'file-1' }])).toEqual([
      'avatar.png',
    ]);
    expect(() => fileDeletionPaths([{ id: 'file-1' }])).toThrow(
      /no longer loaded/,
    );
  });

  it('finds conflicting uploads by full path', () => {
    expect(
      conflictingUploadPaths(files, [
        'avatar.png',
        'avatars/dog.png',
        'new.png',
      ]),
    ).toEqual(['avatar.png', 'avatars/dog.png']);
  });

  it('lets collaborators write files', () => {
    expect(canWriteStorage('collaborator')).toBe(true);
    expect(canWriteStorage('admin')).toBe(true);
    expect(canWriteStorage('owner')).toBe(true);
    expect(canWriteStorage('app-member')).toBe(false);
  });
});
