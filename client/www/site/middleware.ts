import { defineMiddleware } from 'astro/middleware';
import { isStartOwnedPathname } from '@/lib/navigation';
import { dashboardSecurityHeaders } from '@/lib/security-headers.mjs';

// Runtime configuration: a self-hosted object store may use a different origin
// from the dashboard. Validate it once and keep the default policy otherwise.
const storageHeaders = process.env.INSTANT_STORAGE_ORIGIN
  ? dashboardSecurityHeaders(
      process.env.NODE_ENV !== 'production',
      process.env.INSTANT_STORAGE_ORIGIN,
    )
  : undefined;

const PUBLIC_ASSET = /^(?:\/fonts|\/img)(?:\/|$)/;
const STATIC_FILE = /(?:^|\/)[^/]+\.[^/]+$/;
function proxy(request: Request, url: URL) {
  // Constructing from the original request preserves streaming bodies, headers, and method.
  return fetch(new Request(url, request), { redirect: 'manual' });
}

export const onRequest = defineMiddleware(async (context, next) => {
  const { url, request } = context;
  const startOrigin =
    process.env.INSTANT_START_ORIGIN || 'http://localhost:3002';
  const startHandler = /^\/_server(?:Fn)?(?:\/|$)/.test(url.pathname);
  const startAsset =
    url.pathname === '/_build' || url.pathname.startsWith('/_build/');
  let referrerPath = '';
  try {
    referrerPath = new URL(request.headers.get('referer') || '').pathname;
  } catch {
    // Referrer is optional and can be supplied by non-browser clients.
  }
  const startDevAsset =
    /^\/(?:@vite|src|node_modules|assets)(?:\/|$)/.test(url.pathname) &&
    isStartOwnedPathname(referrerPath);
  if (
    isStartOwnedPathname(url.pathname) ||
    startHandler ||
    startAsset ||
    startDevAsset
  ) {
    // Nitro serves production assets from /assets; Vite's public /_build prefix
    // is still required by its development server.
    const pathname =
      startAsset && process.env.NODE_ENV === 'production'
        ? url.pathname.slice('/_build'.length) || '/'
        : url.pathname;
    const response = await proxy(
      request,
      new URL(pathname + url.search, startOrigin),
    );
    if (!storageHeaders) return response;
    const headers = new Headers(response.headers);
    for (const [name, value] of Object.entries(storageHeaders))
      headers.set(name, value);
    return new Response(response.body, {
      status: response.status,
      statusText: response.statusText,
      headers,
    });
  }
  const publicAsset =
    PUBLIC_ASSET.test(url.pathname) ||
    STATIC_FILE.test(url.pathname) ||
    url.pathname.startsWith('/_astro/');
  if (
    process.env.NEXT_PUBLIC_SELF_HOSTED === 'true' &&
    !publicAsset &&
    !/^(?:\/api(?:\/|$)|\/dash(?:\/|$))/.test(url.pathname)
  ) {
    // The adapter can expose an internal origin behind a reverse proxy.
    // Relative redirects retain the browser's public origin.
    return context.redirect('/dash', 307);
  }
  return next();
});
