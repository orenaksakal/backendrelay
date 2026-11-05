import { afterEach, expect, test, vi } from 'vitest';
import { uploadFile } from '../../src/StorageAPI';

afterEach(() => vi.unstubAllGlobals());

test.each(['photos/a.txt', '資料/café 🦀 + & %.txt'])(
  'uploads %s without invalid HTTP headers',
  async (path) => {
    const file = new Blob(['hello']);
    const disposition = `attachment; filename="${path}"`;
    const fetchMock = vi.fn(async (input, options) => {
      const headers = new Headers(options.headers);
      const url = new URL(input);
      expect(url.searchParams.get('path')).toBe(path);
      expect(url.searchParams.get('content-disposition')).toBe(disposition);
      expect(url.searchParams.get('app_id')).toBe('app');
      expect(headers.get('authorization')).toBe('Bearer token');
      expect(options.body).toBe(file);
      expect(headers.get('path')).toBe(path.startsWith('photos') ? path : null);
      return new Response(JSON.stringify({ data: { id: 'file' } }), {
        status: 200,
      });
    });
    vi.stubGlobal('fetch', fetchMock);
    await expect(
      uploadFile({
        apiURI: 'https://api.example',
        appId: 'app',
        path,
        file,
        refreshToken: 'token',
        contentDisposition: disposition,
      }),
    ).resolves.toEqual({ data: { id: 'file' } });
    expect(fetchMock).toHaveBeenCalledOnce();
  },
);
