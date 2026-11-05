import {
  AppWindow,
  Braces,
  DatabaseBackup,
  FolderOpen,
  Home,
  Key,
  KeyRound,
  LayoutGrid,
  Mail,
  RotateCcw,
  Settings,
  ShieldCheck,
  SlidersHorizontal,
  SquareTerminal,
  TableProperties,
  UserRound,
  Users,
  Webhook,
  type LucideIcon,
} from 'lucide-react';

/**
 * Sidebar model for every area (B.6). Hrefs are plain strings because most
 * screens land in later steps; the shell only needs a stable target to link to.
 */
export type NavArea = 'app' | 'workspace' | 'account' | 'operator';

export interface NavItem {
  id: string;
  label: string;
  icon: LucideIcon;
  href: string;
  /** Only the exact path is active; otherwise child paths match too. */
  exact?: boolean;
}

export interface NavGroup {
  id: string;
  /** `aria-hidden`: the nav landmark already names the area (B.6). */
  label?: string;
  items: NavItem[];
}

export interface Navigation {
  area: NavArea;
  groups: NavGroup[];
}

interface AppSectionDefinition {
  id: string;
  label: string;
  icon: LucideIcon;
  /** Path below `/apps/$appId`; empty for the overview. */
  path: string;
  exact?: boolean;
}

const APP_SECTIONS: readonly AppSectionDefinition[] = [
  {
    id: 'overview',
    label: 'Overview',
    icon: Home,
    path: 'overview',
    exact: true,
  },
  {
    id: 'explorer',
    label: 'Explorer',
    icon: TableProperties,
    path: 'explorer',
  },
  { id: 'schema', label: 'Schema', icon: Braces, path: 'schema' },
  { id: 'users', label: 'Users', icon: Users, path: 'users' },
  { id: 'storage', label: 'Storage', icon: FolderOpen, path: 'storage' },
  {
    id: 'permissions',
    label: 'Permissions',
    icon: ShieldCheck,
    path: 'permissions',
  },
  { id: 'auth', label: 'Auth', icon: KeyRound, path: 'auth' },
  {
    id: 'playground',
    label: 'Playground',
    icon: SquareTerminal,
    path: 'playground',
  },
  { id: 'webhooks', label: 'Webhooks', icon: Webhook, path: 'webhooks' },
  {
    id: 'oauth-apps',
    label: 'OAuth apps',
    icon: AppWindow,
    path: 'oauth-apps',
  },
  {
    id: 'backups',
    label: 'Backups',
    icon: DatabaseBackup,
    path: 'backups',
  },
  { id: 'settings', label: 'Settings', icon: Settings, path: 'settings' },
];

const APP_GROUP_IDS: Record<string, readonly string[]> = {
  main: ['overview'],
  data: ['explorer', 'schema', 'users', 'storage'],
  access: ['permissions', 'auth'],
  build: ['playground', 'webhooks', 'oauth-apps'],
  operate: ['backups', 'settings'],
};

const APP_GROUP_LABELS: Record<string, string> = {
  data: 'Data',
  access: 'Access',
  build: 'Build',
  operate: 'Operate',
};

const NAV_LABELS: Record<NavArea, string> = {
  app: 'App',
  workspace: 'Workspace',
  account: 'Account',
  operator: 'Operator',
};

/** Human label for an app section, used by the shortcut sheet (B.7). */
export function appSectionLabel(sectionId: string): string | undefined {
  return APP_SECTIONS.find((section) => section.id === sectionId)?.label;
}

export function appHref(appId: string, sectionId = 'overview'): string {
  const section =
    APP_SECTIONS.find((candidate) => candidate.id === sectionId) ??
    APP_SECTIONS.find((candidate) => candidate.id === 'overview');
  if (section === undefined) throw new Error('Overview section is missing');
  return `/apps/${encodeURIComponent(appId)}/${section.path}`;
}

export function appNavigation(appId: string): Navigation {
  const groups = Object.entries(APP_GROUP_IDS).map(([groupId, ids]) => ({
    id: groupId,
    label: APP_GROUP_LABELS[groupId],
    items: ids.map((id) => {
      const section = APP_SECTIONS.find((candidate) => candidate.id === id);
      if (section === undefined) throw new Error(`Unknown app section ${id}`);
      return {
        id: section.id,
        label: section.label,
        icon: section.icon,
        href: `/apps/${encodeURIComponent(appId)}/${section.path}`,
        exact: section.exact,
      };
    }),
  }));

  return { area: 'app', groups };
}

export interface WorkspaceNavigationOptions {
  workspaceId: string;
  isOrg: boolean;
}

export function workspaceNavigation({
  workspaceId,
  isOrg,
}: WorkspaceNavigationOptions): Navigation {
  const base = `/w/${encodeURIComponent(workspaceId)}`;
  const items: NavItem[] = [
    { id: 'apps', label: 'Apps', icon: LayoutGrid, href: base, exact: true },
  ];
  if (isOrg) {
    items.push(
      {
        id: 'members',
        label: 'Members',
        icon: Users,
        href: `${base}/members`,
      },
      {
        id: 'settings',
        label: 'Settings',
        icon: Settings,
        href: `${base}/settings`,
      },
    );
  }
  return { area: 'workspace', groups: [{ id: 'workspace', items }] };
}

export function accountNavigation(): Navigation {
  return {
    area: 'account',
    groups: [
      {
        id: 'account',
        items: [
          {
            id: 'profile',
            label: 'Profile',
            icon: UserRound,
            href: '/account/profile',
          },
          {
            id: 'tokens',
            label: 'Tokens',
            icon: Key,
            href: '/account/tokens',
          },
          {
            id: 'oauth-apps',
            label: 'OAuth apps',
            icon: AppWindow,
            href: '/account/oauth-apps',
          },
          {
            id: 'invites',
            label: 'Invites',
            icon: Mail,
            href: '/account/invites',
          },
        ],
      },
    ],
  };
}

export function operatorNavigation(): Navigation {
  return {
    area: 'operator',
    groups: [
      {
        id: 'operator',
        items: [
          {
            id: 'settings',
            label: 'Settings',
            icon: SlidersHorizontal,
            href: '/operator/settings',
          },
          {
            id: 'restores',
            label: 'Restores',
            icon: RotateCcw,
            href: '/operator/restores',
          },
        ],
      },
    ],
  };
}

export function navLandmarkLabel(area: NavArea): string {
  return NAV_LABELS[area];
}

export function navItems(navigation: Navigation): NavItem[] {
  return navigation.groups.flatMap((group) => group.items);
}

/**
 * The most specific item whose path contains the current location. `/webhooks`
 * stays active on `/webhooks/$webhookId`, while Overview only matches itself.
 */
export function activeNavItem(
  navigation: Navigation,
  pathname: string,
): NavItem | undefined {
  const normalised = stripTrailingSlash(pathname);
  let best: NavItem | undefined;
  for (const item of navItems(navigation)) {
    const target = stripTrailingSlash(item.href);
    const matches = item.exact
      ? normalised === target
      : normalised === target || normalised.startsWith(`${target}/`);
    if (!matches) continue;
    if (
      best === undefined ||
      target.length > stripTrailingSlash(best.href).length
    ) {
      best = item;
    }
  }
  return best;
}

/** `/w/personal` → `personal`; `null` outside a workspace route. */
export function workspaceIdFromPath(pathname: string): string | null {
  const match = /^\/w\/([^/]+)/.exec(pathname);
  const encoded = match?.[1];
  if (encoded === undefined) return null;
  try {
    return decodeURIComponent(encoded);
  } catch {
    return encoded;
  }
}

/** Falls back to a humanised last segment for screens outside the nav model. */
export function pageLabelFromPath(pathname: string): string {
  const segment = pathname
    .split('/')
    .filter((part) => part !== '')
    .at(-1);
  if (segment === undefined) return 'Home';
  let decoded = segment;
  try {
    decoded = decodeURIComponent(segment);
  } catch {
    /* Keep the raw segment. */
  }
  const words = decoded.replace(/[-_]+/g, ' ');
  return words.charAt(0).toUpperCase() + words.slice(1);
}

/** `{Page} · {App} · BackendRelay` (D3.4). */
export function documentTitle(
  page: string | undefined,
  appTitle: string | undefined,
): string {
  return [page, appTitle, 'BackendRelay']
    .filter((part): part is string => part !== undefined && part !== '')
    .join(' · ');
}

function stripTrailingSlash(value: string): string {
  return value.endsWith('/') && value !== '/' ? value.slice(0, -1) : value;
}
