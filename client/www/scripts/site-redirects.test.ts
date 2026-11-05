import assert from 'node:assert/strict';
import test from 'node:test';
import { createContext } from 'astro/middleware';
import { onRequest } from '../site/middleware';

test('same-site redirects remain on the public host behind a reverse proxy', async () => {
  const previous = process.env.NEXT_PUBLIC_SELF_HOSTED;
  process.env.NEXT_PUBLIC_SELF_HOSTED = 'true';
  try {
    for (const [path, expected, status] of [
      ['/', '/dash', 307],
      ['/signin', '/dash', 307],
      ['/docs/auth/apple?method=native', '/dash', 307],
    ] as const) {
      const context = createContext({
        defaultLocale: '',
        locals: {},
        request: new Request(`https://localhost${path}`, {
          headers: { host: 'dashboard.example.com' },
        }),
      });
      const response = await onRequest(
        context,
        async () => new Response('next'),
      );
      assert.ok(response instanceof Response);
      assert.equal(response.status, status, path);
      assert.equal(response.headers.get('location'), expected, path);
      assert.equal(
        new URL(
          response.headers.get('location')!,
          'https://dashboard.example.com',
        ).origin,
        'https://dashboard.example.com',
      );
    }
  } finally {
    if (previous === undefined) delete process.env.NEXT_PUBLIC_SELF_HOSTED;
    else process.env.NEXT_PUBLIC_SELF_HOSTED = previous;
  }
});
