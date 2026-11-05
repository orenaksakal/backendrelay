// The existing SSR hydration protocol emits inline scripts and styles. Keep
// that compatibility exception explicit until nonce propagation is implemented.
// Backend origins are runtime-configurable (including local HTTP self-hosting).
export function dashboardSecurityHeaders(
  development = false,
  storageOrigin = '',
) {
  let trustedStorage = '';
  if (storageOrigin) {
    const parsed = new URL(storageOrigin);
    if (
      !['http:', 'https:'].includes(parsed.protocol) ||
      parsed.username ||
      parsed.password ||
      parsed.pathname !== '/' ||
      parsed.search ||
      parsed.hash ||
      parsed.hostname.includes('*') ||
      /[\s;\\]/.test(storageOrigin)
    ) {
      throw new Error(
        'INSTANT_STORAGE_ORIGIN must be one exact HTTP(S) origin',
      );
    }
    trustedStorage = ` ${parsed.origin}`;
  }
  const policy = [
    "default-src 'self'",
    `script-src 'self' 'unsafe-inline'${development ? " 'unsafe-eval'" : ''} https://cdn.jsdelivr.net`,
    "style-src 'self' 'unsafe-inline' https://fonts.googleapis.com https://cdn.jsdelivr.net",
    "connect-src 'self' http: https: ws: wss:",
    `img-src 'self' data: blob: https:${trustedStorage}`,
    "font-src 'self' data: https://fonts.gstatic.com https://cdn.jsdelivr.net",
    "worker-src 'self' blob:",
    "frame-src 'self' https://accounts.google.com",
    "frame-ancestors 'none'",
    "base-uri 'self'",
    "object-src 'none'",
    "form-action 'self' https:",
  ].join('; ');
  return {
    'Content-Security-Policy': policy,
    'X-Content-Type-Options': 'nosniff',
    'X-Frame-Options': 'DENY',
    'Referrer-Policy': 'no-referrer',
    'Permissions-Policy': 'camera=(), microphone=(), geolocation=()',
  };
}
