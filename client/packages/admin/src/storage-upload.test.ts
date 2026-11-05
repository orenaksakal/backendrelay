import { afterEach, expect, test, vi } from 'vitest';
import { init } from './index.js';

const originalFetch = globalThis.fetch;
afterEach(() => {
  globalThis.fetch = originalFetch;
});

test.each(['photos/a.txt', '資料/café 🦀 + & %.txt'])(
  'uploads %s with lossless query metadata',
  async (path) => {
    const file = Buffer.from('hello');
    const disposition = `attachment; filename="${path}"`;
    const fetchMock = vi.fn(async (input, options) => {
      const headers = new Headers(options.headers);
      const url = new URL(input);
      expect(url.searchParams.get('path')).toBe(path);
      expect(url.searchParams.get('content-disposition')).toBe(disposition);
      expect(headers.get('authorization')).toBe('Bearer secret');
      expect(headers.get('path')).toBe(path.startsWith('photos') ? path : null);
      expect(options.body).toBe(file);
      return new Response(JSON.stringify({ data: { id: 'file' } }), {
        status: 200,
      });
    });
    vi.stubGlobal('fetch', fetchMock);
    const db = init({
      appId: 'app',
      adminToken: 'secret',
      apiURI: 'https://api.example',
    });
    await expect(
      db.storage.uploadFile(path, file, { contentDisposition: disposition }),
    ).resolves.toEqual({ data: { id: 'file' } });
    expect(fetchMock).toHaveBeenCalledOnce();
  },
);
