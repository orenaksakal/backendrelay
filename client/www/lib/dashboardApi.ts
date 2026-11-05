/** Route management traffic privately without changing SDK/runtime URLs or signed downloads. */
export function routeDashboardURL(
  input: string,
  apiURI: string,
  dashboardApiURI: string,
): string {
  if (apiURI === dashboardApiURI) return input;
  let url: URL;
  try {
    url = new URL(input);
  } catch {
    return input;
  }
  const api = new URL(apiURI);
  const basePath = api.pathname.replace(/\/$/, '');
  const path = url.pathname.slice(basePath.length);
  const websocket = url.protocol === 'ws:' || url.protocol === 'wss:';
  const protocol = websocket
    ? url.protocol === 'wss:'
      ? 'https:'
      : 'http:'
    : url.protocol;
  if (
    protocol !== api.protocol ||
    url.host !== api.host ||
    !url.pathname.startsWith(basePath + '/') ||
    !['/dash', '/superadmin', '/platform/oauth'].some(
      (prefix) => path === prefix || path.startsWith(prefix + '/'),
    )
  )
    return input;
  const target = new URL(dashboardApiURI);
  if (websocket)
    target.protocol = target.protocol === 'https:' ? 'wss:' : 'ws:';
  target.pathname = target.pathname.replace(/\/$/, '') + path;
  target.search = url.search;
  target.hash = url.hash;
  return target.toString();
}
