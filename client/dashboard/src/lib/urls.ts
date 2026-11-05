/**
 * The realtime endpoint the client SDK connects to, derived from the API origin
 * the way `www` derived it: same host, `ws`/`wss` scheme and `/runtime/session`
 * (C.5 connect sheet, C.20 devtool).
 */
export function websocketUrl(apiBaseUrl: string): string | undefined {
  try {
    const url = new URL(apiBaseUrl);
    if (url.protocol !== 'http:' && url.protocol !== 'https:') return undefined;
    url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
    url.pathname = '/runtime/session';
    url.search = '';
    url.hash = '';
    return url.toString();
  } catch {
    return undefined;
  }
}
