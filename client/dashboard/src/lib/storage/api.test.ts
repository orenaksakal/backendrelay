import { beforeEach, describe, expect, it, vi } from 'vitest';
import { apiFetch } from '@/lib/api/client';
import { storageEndpoints } from '@/lib/api/endpoints';
import { deleteStorageFiles, uploadStorageFile } from './api';

vi.mock('@/lib/api/client', () => ({
  apiFetch: vi.fn(() => Promise.resolve({})),
}));

const apiFetchMock = vi.mocked(apiFetch);

describe('storage API', () => {
  beforeEach(() => {
    apiFetchMock.mockClear();
  });

  it('uploads the raw file with its path as a query parameter', async () => {
    const file = new File(['sample'], 'cat.png', { type: 'image/png' });
    const controller = new AbortController();

    await uploadStorageFile({
      appId: 'app-1',
      path: 'avatars/2026/cat.png',
      file,
      signal: controller.signal,
    });

    expect(apiFetchMock).toHaveBeenCalledWith(storageEndpoints.upload, {
      params: { appId: 'app-1', path: 'avatars/2026/cat.png' },
      body: file,
      signal: controller.signal,
    });
  });

  it('deletes by filename through the dashboard endpoint', async () => {
    await deleteStorageFiles({
      appId: 'app-1',
      filenames: ['avatars/cat.png', 'dog.png'],
    });

    expect(apiFetchMock).toHaveBeenCalledWith(storageEndpoints.deleteFiles, {
      params: { appId: 'app-1' },
      body: { filenames: ['avatars/cat.png', 'dog.png'] },
    });
  });
});
