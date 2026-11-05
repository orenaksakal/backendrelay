import type { BackupsManager } from '@instantdb/platform';
import {
  afterEach,
  beforeEach,
  describe,
  expect,
  it,
  vi,
  type Mock,
} from 'vitest';
import {
  downloadAllFiles,
  downloadFiles,
  type BackupManifest,
} from './download';

const MANIFEST: BackupManifest = {
  files: [
    { name: 'config.json', size: 12 },
    { name: 'entities/todos.jsonl', size: 34 },
  ],
  storageFiles: [
    {
      locationId: 'loc-1',
      path: 'images/me.png',
      url: 'https://objects.test/loc-1',
    },
  ],
};

const FILES = downloadFiles(MANIFEST);

const URLS = [
  'https://objects.test/config.json',
  'https://objects.test/entities/todos.jsonl',
  'https://objects.test/loc-1',
];

const createObjectUrl = vi.fn<(blob: Blob) => string>();
const revokeObjectUrl = vi.fn<(url: string) => void>();
let objectUrls = 0;

function fakeManager(): {
  manager: BackupsManager;
  getFileUrl: Mock<(backupId: string, name: string) => Promise<string>>;
} {
  const getFileUrl = vi.fn((_backupId: string, name: string) =>
    Promise.resolve(`https://objects.test/${name}`),
  );
  return {
    manager: { getFileUrl } as unknown as BackupsManager,
    getFileUrl,
  };
}

function okResponse(): Response {
  return {
    ok: true,
    status: 200,
    blob: () => Promise.resolve(new Blob(['payload'])),
  } as unknown as Response;
}

beforeEach(() => {
  objectUrls = 0;
  createObjectUrl.mockReset();
  revokeObjectUrl.mockReset();
  createObjectUrl.mockImplementation(() => `blob:file-${String(++objectUrls)}`);
  Object.defineProperty(URL, 'createObjectURL', {
    configurable: true,
    value: createObjectUrl,
  });
  Object.defineProperty(URL, 'revokeObjectURL', {
    configurable: true,
    value: revokeObjectUrl,
  });
});

afterEach(() => {
  Reflect.deleteProperty(URL, 'createObjectURL');
  Reflect.deleteProperty(URL, 'revokeObjectURL');
});

describe('backup downloads', () => {
  it('fetches every file and saves each through a blob URL', async () => {
    const fetchMock = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(okResponse());
    const clicked: HTMLAnchorElement[] = [];
    vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(function (
      this: HTMLAnchorElement,
    ) {
      clicked.push(this);
    });
    const progress: Array<[number, number]> = [];
    const { manager, getFileUrl } = fakeManager();
    const controller = new AbortController();

    await downloadAllFiles({
      manager,
      backupId: 'backup-1',
      manifest: MANIFEST,
      files: FILES,
      signal: controller.signal,
      onProgress: (done, total) => progress.push([done, total]),
    });

    expect(getFileUrl.mock.calls).toEqual([
      ['backup-1', 'config.json'],
      ['backup-1', 'entities/todos.jsonl'],
    ]);
    expect(fetchMock.mock.calls.map(([url]) => url)).toEqual(URLS);
    expect(fetchMock.mock.calls.map(([, init]) => init?.signal)).toEqual([
      controller.signal,
      controller.signal,
      controller.signal,
    ]);

    expect(createObjectUrl).toHaveBeenCalledTimes(FILES.length);
    for (const [blob] of createObjectUrl.mock.calls) {
      expect(blob).toBeInstanceOf(Blob);
    }
    const blobUrls = createObjectUrl.mock.results.map((result) =>
      String(result.value),
    );
    expect(blobUrls.every((url) => url.startsWith('blob:'))).toBe(true);

    expect(clicked.map((anchor) => anchor.download)).toEqual([
      'config.json',
      'todos.jsonl',
      'me.png',
    ]);
    expect(clicked.map((anchor) => anchor.href)).toEqual(blobUrls);
    expect(revokeObjectUrl.mock.calls.map(([url]) => url)).toEqual(blobUrls);
    expect(progress).toEqual([
      [0, 3],
      [1, 3],
      [2, 3],
      [3, 3],
    ]);
  });

  it('reports a failed fetch without clicking or saving the file', async () => {
    const fetchMock = vi.spyOn(globalThis, 'fetch').mockResolvedValue({
      ok: false,
      status: 503,
    } as unknown as Response);
    const click = vi
      .spyOn(HTMLAnchorElement.prototype, 'click')
      .mockImplementation(() => undefined);
    const { manager } = fakeManager();

    await expect(
      downloadAllFiles({
        manager,
        backupId: 'backup-1',
        manifest: MANIFEST,
        files: FILES,
      }),
    ).rejects.toThrow('HTTP 503 for config.json.');

    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(createObjectUrl).not.toHaveBeenCalled();
    expect(revokeObjectUrl).not.toHaveBeenCalled();
    expect(click).not.toHaveBeenCalled();
  });
});
