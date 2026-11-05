/**
 * Old dashboard URLs that the CLI, invite emails and bookmarks still use
 * (B.6). The table lives here so `/dash`, `/dash/*` and
 * `/intern/deployment-settings` cannot drift apart, and so every row is
 * covered by one table-driven unit test.
 */

/** Only used to parse relative hrefs; never navigated to. */
const LEGACY_ORIGIN = 'https://legacy.backendrelay.invalid';

/** Old `T` tab ids and where that section lives in the new app shell. */
const APP_SECTION_BY_TAB: Record<string, string> = {
  home: 'overview',
  explorer: 'explorer',
  schema: 'schema',
  perms: 'permissions',
  auth: 'auth',
  webhooks: 'webhooks',
  backups: 'backups',
  repl: 'playground',
  sandbox: 'playground',
  admin: 'settings',
  billing: 'settings',
  'oauth-apps': 'oauth-apps',
};

/** True for the exact `/dash` index, where the search-only rows apply. */
export function isLegacyDashIndex(pathname: string): boolean {
  return stripTrailingSlash(pathname) === '/dash';
}

/**
 * The B.6 legacy URL table. `href` is a path with its query string, e.g.
 * `/dash?app=A&t=explorer`; the result is the new path to replace it with.
 * Unknown `/dash/*` paths fall back to `/` rather than a 404, because the old
 * dashboard treated them as the default screen.
 */
export function legacyRedirect(href: string): string {
  const url = new URL(href, LEGACY_ORIGIN);
  const pathname = stripTrailingSlash(url.pathname);

  if (pathname === '/intern/deployment-settings') {
    return '/operator/settings';
  }
  if (pathname !== '/dash' && !pathname.startsWith('/dash/')) {
    return '/';
  }

  switch (pathname) {
    case '/dash/new':
      return '/new';
    case '/dash/onboarding':
      return '/welcome';
    case '/dash/user-settings':
      return userSettingsRedirect(url.searchParams);
    case '/dash/org':
      return orgRedirect(url.searchParams);
    case '/dash':
      return dashRedirect(url.searchParams);
    default:
      return '/';
  }
}

function dashRedirect(search: URLSearchParams): string {
  const ticket =
    readSearchValue(search, 'ticket') ??
    readSearchValue(search, '_cli_oauth_ticket');
  if (ticket !== undefined) {
    return `/cli/approve?${new URLSearchParams({ ticket }).toString()}`;
  }

  const app = readSearchValue(search, 'app');
  if (app !== undefined) {
    const tab = search.get('t');
    const section = tab === null ? undefined : APP_SECTION_BY_TAB[tab];
    return `/apps/${encodeURIComponent(app)}/${section ?? 'overview'}`;
  }

  switch (search.get('s')) {
    case 'new':
      return '/new';
    case 'invites':
      return '/account/invites';
    case 'personal-access-tokens':
      return '/account/tokens';
  }

  const org = readSearchValue(search, 'org');
  return org === undefined ? '/' : `/w/${encodeURIComponent(org)}`;
}

function userSettingsRedirect(search: URLSearchParams): string {
  switch (search.get('tab')) {
    case 'token':
      return '/account/tokens';
    case 'oauth':
      return '/account/oauth-apps';
    default:
      return '/account/profile';
  }
}

function orgRedirect(search: URLSearchParams): string {
  const org = readSearchValue(search, 'org');
  if (org === undefined) return '/';
  return search.get('tab') === 'members'
    ? `/w/${encodeURIComponent(org)}/members`
    : `/w/${encodeURIComponent(org)}/settings`;
}

function readSearchValue(
  search: URLSearchParams,
  key: string,
): string | undefined {
  const value = search.get(key);
  return value === null || value === '' ? undefined : value;
}

function stripTrailingSlash(pathname: string): string {
  if (pathname === '/') return pathname;
  return pathname.replace(/\/+$/, '');
}
