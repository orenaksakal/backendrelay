import { useRouter, type NavigateOptions } from '@tanstack/react-router';
import {
  ArrowRight,
  AppWindow,
  Check,
  Clipboard,
  Command as CommandIcon,
  Plus,
  Settings,
  UserRound,
} from 'lucide-react';
import { useEffect, useMemo, useState } from 'react';
import type * as React from 'react';
import {
  CommandDialog,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
  CommandShortcut,
} from '@/components/ui/command';
import { copyText } from '@/lib/clipboard';
import { getRuntimeConfig } from '@/lib/runtime-config';
import {
  appHref,
  appNavigation,
  navItems,
  type NavItem,
} from '@/lib/navigation';
import { appSectionShortcutKeys, formatShortcut } from '@/lib/shortcuts';
import { toastManager } from '@/lib/toast';
import { websocketUrl } from '@/lib/urls';
import { truncateId } from '@/lib/format';
import { useTheme } from '@/lib/theme';
import {
  readPinnedAppIds,
  readRecentAppIds,
  RECENT_APP_LIMIT,
  rememberRecentApp,
} from '@/lib/app-usage';
import type { App } from '@/lib/api/endpoints/schemas';
import { workspaceIdForApp } from '@/lib/workspace/app-loader';
import {
  PERSONAL_WORKSPACE_TITLE,
  type Workspace,
} from '@/lib/workspace/workspaces';

/** `all` is ⌘K; the picker modes come from the breadcrumb switchers (B.6/B.7). */
export type PaletteMode = 'all' | 'apps' | 'workspaces';

export interface PalettePageAction {
  id: string;
  label: string;
  /** Display caps of the shortcut that runs the same action, if any. */
  shortcut?: string;
  run: () => void;
}

export interface CommandPaletteProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  mode: PaletteMode;
  /** The app whose pages, namespaces and URLs the palette offers. */
  app?: App;
  /** Keeps the current section when switching apps from a breadcrumb (B.6). */
  sectionId?: string;
  workspaceId?: string;
  workspaces: Workspace[];
  apps: App[];
  superuser: boolean;
  /** Page actions for the "This page" group; screens add these later. */
  pageActions?: PalettePageAction[];
  /** Namespaces exposed by the mounted screen (D6.1 fills this in). */
  namespaces?: string[];
  onToggleSidebar: () => void;
}

const UUID_PATTERN =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/**
 * The ⌘K palette (B.7): page actions, navigation, apps with recent and pinned
 * ordering, namespaces, create/copy actions and preferences. Picker modes show
 * only apps or only workspaces, and pasting an app UUID opens that app.
 */
export function CommandPalette({
  open,
  onOpenChange,
  mode,
  app,
  sectionId,
  workspaceId,
  workspaces,
  apps,
  superuser,
  pageActions = [],
  namespaces = [],
  onToggleSidebar,
}: CommandPaletteProps) {
  const router = useRouter();
  const { theme, setTheme } = useTheme();
  const [query, setQuery] = useState('');
  const [recentIds, setRecentIds] = useState<string[]>([]);
  const [pinnedIds, setPinnedIds] = useState<string[]>([]);

  useEffect(() => {
    if (!open) return;
    // eslint-disable-next-line react-hooks/set-state-in-effect -- opening the palette reads recent and pinned apps from localStorage
    setQuery('');
    setRecentIds(readRecentAppIds());
    setPinnedIds(readPinnedAppIds(workspaces.map((workspace) => workspace.id)));
  }, [open, workspaces]);

  const close = () => {
    onOpenChange(false);
  };

  const go = (href: string) => {
    close();
    // Screens land in later steps; `href` is the documented way to reach a
    // path the generated route table cannot type yet.
    void router.navigate({ href } as NavigateOptions);
  };

  const appHrefFor = (target: App) =>
    appHref(target.id, sectionId ?? 'overview');

  const openApp = (target: App) => {
    rememberRecentApp(target.id);
    go(appHrefFor(target));
  };

  const copy = (label: string, value: string) => {
    close();
    void copyText(value).then((copied) => {
      if (!copied) {
        toastManager.add({
          type: 'error',
          title: `Couldn't copy ${label}`,
          priority: 'high',
          timeout: 0,
        });
        return;
      }
      toastManager.add({
        type: 'success',
        title: `Copied ${label}`,
      });
    });
  };

  const workspaceTitle = (target: App) => {
    const id = workspaceIdForApp(target);
    const workspace = workspaces.find((candidate) => candidate.id === id);
    return workspace?.title ?? target.org?.title ?? PERSONAL_WORKSPACE_TITLE;
  };

  const recentApps = useMemo(() => {
    const byId = new Map(apps.map((candidate) => [candidate.id, candidate]));
    return recentIds
      .map((id) => byId.get(id))
      .filter((candidate): candidate is App => candidate !== undefined)
      .slice(0, RECENT_APP_LIMIT);
  }, [apps, recentIds]);

  const pinnedApps = useMemo(() => {
    const pinned = new Set(pinnedIds);
    return apps.filter((candidate) => pinned.has(candidate.id));
  }, [apps, pinnedIds]);

  const sections: NavItem[] =
    app === undefined ? [] : navItems(appNavigation(app.id));

  const appsHomeHref =
    workspaceId === undefined
      ? undefined
      : `/w/${encodeURIComponent(workspaceId)}`;

  const apiBaseUrl = getRuntimeConfig()?.apiBaseUrl;
  const realtimeUrl =
    apiBaseUrl === undefined ? undefined : websocketUrl(apiBaseUrl);

  const handleQueryChange = (value: string) => {
    setQuery(value);
    if (mode === 'workspaces') return;
    const trimmed = value.trim();
    if (!UUID_PATTERN.test(trimmed)) return;
    const match = apps.find(
      (candidate) => candidate.id.toLowerCase() === trimmed.toLowerCase(),
    );
    if (match !== undefined) openApp(match);
  };

  // ⇧↵ opens the highlighted row in a new tab, like the footer promises.
  // cmdk keeps focus in the input, so both the input and the list listen.
  const openSelectedInNewTab = (event: React.KeyboardEvent<HTMLElement>) => {
    if (event.key !== 'Enter' || !event.shiftKey) return;
    const root = event.currentTarget.closest('[cmdk-root]');
    const selected = root?.querySelector('[cmdk-item][data-selected="true"]');
    const href = selected?.getAttribute('data-href');
    if (href === null || href === undefined) return;
    event.preventDefault();
    event.stopPropagation();
    close();
    window.open(href, '_blank', 'noopener,noreferrer');
  };

  const showNavigation = mode === 'all';
  const showApps = mode === 'all' || mode === 'apps';
  const showWorkspaces = mode === 'workspaces';

  const title =
    mode === 'apps'
      ? 'Switch app'
      : mode === 'workspaces'
        ? 'Switch workspace'
        : 'Command palette';

  const searchLabel =
    mode === 'workspaces'
      ? 'Search workspaces'
      : mode === 'apps'
        ? 'Search apps'
        : 'Search apps and actions';

  return (
    <CommandDialog
      open={open}
      onOpenChange={(next, details) => {
        // Esc clears the query first; the next Esc closes the dialog (B.7).
        if (!next && details.reason === 'escape-key' && query !== '') {
          details.cancel();
          setQuery('');
          return;
        }
        onOpenChange(next);
      }}
      title={title}
      className="sm:max-w-[560px]"
    >
      <CommandInput
        value={query}
        onValueChange={handleQueryChange}
        placeholder={`${searchLabel}…`}
        aria-label={searchLabel}
        onKeyDown={openSelectedInNewTab}
      />
      <CommandList onKeyDown={openSelectedInNewTab}>
        <CommandEmpty>No results found.</CommandEmpty>

        {showNavigation && pageActions.length > 0 ? (
          <CommandGroup heading="This page">
            {pageActions.map((action) => (
              <CommandItem
                key={action.id}
                value={`page:${action.id}`}
                onSelect={() => {
                  close();
                  action.run();
                }}
              >
                <ArrowRight aria-hidden="true" />
                <span className="truncate">{action.label}</span>
                {action.shortcut === undefined ? null : (
                  <CommandShortcut>{action.shortcut}</CommandShortcut>
                )}
              </CommandItem>
            ))}
          </CommandGroup>
        ) : null}

        {showNavigation ? (
          <CommandGroup heading="Navigate">
            {sections.map((section) => {
              const keys = appSectionShortcutKeys(section.id);
              const Icon = section.icon;
              return (
                <CommandItem
                  key={section.id}
                  value={`section:${section.id}`}
                  data-href={section.href}
                  onSelect={() => {
                    go(section.href);
                  }}
                >
                  <Icon aria-hidden="true" />
                  <span className="truncate">Go to {section.label}</span>
                  {keys === undefined ? null : (
                    <CommandShortcut>{formatShortcut(keys)}</CommandShortcut>
                  )}
                </CommandItem>
              );
            })}
            {appsHomeHref === undefined ? null : (
              <CommandItem
                value="section:apps-home"
                data-href={appsHomeHref}
                onSelect={() => {
                  go(appsHomeHref);
                }}
              >
                <AppWindow aria-hidden="true" />
                <span className="truncate">Apps home</span>
                <CommandShortcut>{formatShortcut('g h')}</CommandShortcut>
              </CommandItem>
            )}
            <CommandItem
              value="section:account"
              data-href="/account/profile"
              onSelect={() => {
                go('/account/profile');
              }}
            >
              <UserRound aria-hidden="true" />
              <span className="truncate">Account</span>
            </CommandItem>
            {superuser ? (
              <CommandItem
                value="section:operator"
                data-href="/operator/settings"
                onSelect={() => {
                  go('/operator/settings');
                }}
              >
                <Settings aria-hidden="true" />
                <span className="truncate">Operator</span>
              </CommandItem>
            ) : null}
          </CommandGroup>
        ) : null}

        {showWorkspaces ? (
          <CommandGroup heading="Workspaces">
            {workspaces.map((workspace) => (
              <CommandItem
                key={workspace.id}
                value={`workspace:${workspace.id}`}
                keywords={[workspace.title]}
                data-href={`/w/${encodeURIComponent(workspace.id)}`}
                onSelect={() => {
                  go(`/w/${encodeURIComponent(workspace.id)}`);
                }}
              >
                <span className="truncate">{workspace.title}</span>
                {workspace.kind === 'org' ? (
                  <span className="text-muted-foreground ml-auto text-xs">
                    Org
                  </span>
                ) : null}
              </CommandItem>
            ))}
          </CommandGroup>
        ) : null}

        {showApps && recentApps.length > 0 ? (
          <CommandGroup heading="Recent apps">
            {recentApps.map((target) => (
              <AppRow
                key={target.id}
                group="recent"
                app={target}
                href={appHrefFor(target)}
                workspaceTitle={workspaceTitle(target)}
                onSelect={() => {
                  openApp(target);
                }}
              />
            ))}
          </CommandGroup>
        ) : null}

        {showApps && pinnedApps.length > 0 ? (
          <CommandGroup heading="Pinned apps">
            {pinnedApps.map((target) => (
              <AppRow
                key={target.id}
                group="pinned"
                app={target}
                href={appHrefFor(target)}
                workspaceTitle={workspaceTitle(target)}
                onSelect={() => {
                  openApp(target);
                }}
              />
            ))}
          </CommandGroup>
        ) : null}

        {showApps && apps.length > 0 ? (
          <CommandGroup heading="Apps">
            {apps.map((target) => (
              <AppRow
                key={target.id}
                group="app"
                app={target}
                href={appHrefFor(target)}
                workspaceTitle={workspaceTitle(target)}
                onSelect={() => {
                  openApp(target);
                }}
              />
            ))}
          </CommandGroup>
        ) : null}

        {showNavigation && app !== undefined && namespaces.length > 0 ? (
          <CommandGroup heading="Namespaces">
            {namespaces.map((namespaceName) => {
              const base = appHref(app.id, 'explorer');
              const href = `${base}?ns=${encodeURIComponent(namespaceName)}`;
              return (
                <CommandItem
                  key={namespaceName}
                  value={`namespace:${namespaceName}`}
                  data-href={href}
                  onSelect={() => {
                    go(href);
                  }}
                >
                  <span className="truncate font-mono text-sm">
                    {namespaceName}
                  </span>
                </CommandItem>
              );
            })}
          </CommandGroup>
        ) : null}

        {showNavigation ? (
          <CommandGroup heading="Create">
            <CommandItem
              value="create:app"
              data-href="/new"
              onSelect={() => {
                go('/new');
              }}
            >
              <Plus aria-hidden="true" />
              <span className="truncate">Create app</span>
            </CommandItem>
            <CommandItem
              value="create:org"
              data-href="/new"
              onSelect={() => {
                go('/new');
              }}
            >
              <Plus aria-hidden="true" />
              <span className="truncate">Create org</span>
            </CommandItem>
            {app === undefined ? null : (
              <>
                <CommandItem
                  value="create:webhook"
                  data-href={appHref(app.id, 'webhooks')}
                  onSelect={() => {
                    go(appHref(app.id, 'webhooks'));
                  }}
                >
                  <Plus aria-hidden="true" />
                  <span className="truncate">Create webhook</span>
                </CommandItem>
                <CommandItem
                  value="create:backup"
                  data-href={appHref(app.id, 'backups')}
                  onSelect={() => {
                    go(appHref(app.id, 'backups'));
                  }}
                >
                  <Plus aria-hidden="true" />
                  <span className="truncate">Create backup</span>
                </CommandItem>
              </>
            )}
            <CommandItem
              value="create:token"
              data-href="/account/tokens"
              onSelect={() => {
                go('/account/tokens');
              }}
            >
              <Plus aria-hidden="true" />
              <span className="truncate">Create personal access token</span>
            </CommandItem>
          </CommandGroup>
        ) : null}

        {showNavigation && app !== undefined ? (
          <CommandGroup heading="Copy">
            <CommandItem
              value="copy:app-id"
              onSelect={() => {
                copy('app ID', app.id);
              }}
            >
              <Clipboard aria-hidden="true" />
              <span className="truncate">Copy app ID</span>
            </CommandItem>
            {apiBaseUrl === undefined ? null : (
              <CommandItem
                value="copy:api-url"
                onSelect={() => {
                  copy('API URL', apiBaseUrl);
                }}
              >
                <Clipboard aria-hidden="true" />
                <span className="truncate">Copy API URL</span>
              </CommandItem>
            )}
            {realtimeUrl === undefined ? null : (
              <CommandItem
                value="copy:websocket-url"
                onSelect={() => {
                  copy('WebSocket URL', realtimeUrl);
                }}
              >
                <Clipboard aria-hidden="true" />
                <span className="truncate">Copy WebSocket URL</span>
              </CommandItem>
            )}
          </CommandGroup>
        ) : null}

        {showNavigation ? (
          <CommandGroup heading="Preferences">
            <CommandItem
              value="theme:system"
              onSelect={() => {
                setTheme('system');
                close();
              }}
            >
              <CommandIcon aria-hidden="true" />
              <span className="truncate">Theme: System</span>
              {theme === 'system' ? <CurrentLabel /> : null}
            </CommandItem>
            <CommandItem
              value="theme:light"
              onSelect={() => {
                setTheme('light');
                close();
              }}
            >
              <CommandIcon aria-hidden="true" />
              <span className="truncate">Theme: Light</span>
              {theme === 'light' ? <CurrentLabel /> : null}
            </CommandItem>
            <CommandItem
              value="theme:dark"
              onSelect={() => {
                setTheme('dark');
                close();
              }}
            >
              <CommandIcon aria-hidden="true" />
              <span className="truncate">Theme: Dark</span>
              {theme === 'dark' ? <CurrentLabel /> : null}
            </CommandItem>
            <CommandItem
              value="preference:sidebar"
              onSelect={() => {
                close();
                onToggleSidebar();
              }}
            >
              <Settings aria-hidden="true" />
              <span className="truncate">Toggle sidebar</span>
              <CommandShortcut>[</CommandShortcut>
            </CommandItem>
          </CommandGroup>
        ) : null}
      </CommandList>
      <div className="border-border text-muted-foreground flex shrink-0 flex-wrap items-center gap-x-2 gap-y-1 border-t px-3 py-1.5 text-xs">
        <span>↑↓ Navigate</span>
        <span aria-hidden="true">·</span>
        <span>↵ Open</span>
        <span aria-hidden="true">·</span>
        <span>⇧↵ Open in new tab</span>
        <span aria-hidden="true">·</span>
        <span>Esc Close</span>
      </div>
    </CommandDialog>
  );
}

interface AppRowProps {
  /** Distinguishes the same app in the recent, pinned and all lists. */
  group: 'recent' | 'pinned' | 'app';
  app: App;
  /** Where the row goes; switching apps keeps the current section (B.6). */
  href: string;
  workspaceTitle: string;
  onSelect: () => void;
}

/**
 * One app row: title, workspace and the truncated ID. The ID is not the
 * copyable `IdChip` here because the row itself is the listbox option; "Copy
 * app ID" in the Copy group does the copying.
 */
function AppRow({ group, app, href, workspaceTitle, onSelect }: AppRowProps) {
  return (
    <CommandItem
      value={`${group}:${app.id}`}
      keywords={[app.title, workspaceTitle]}
      data-href={href}
      onSelect={onSelect}
    >
      <span className="truncate">{app.title}</span>
      <span className="text-muted-foreground truncate text-xs">
        {workspaceTitle}
      </span>
      <span
        title={app.id}
        className="text-muted-foreground ml-auto font-mono text-xs"
      >
        {truncateId(app.id)}
      </span>
    </CommandItem>
  );
}

function CurrentLabel() {
  return (
    <span className="text-muted-foreground ml-auto inline-flex items-center gap-1 text-xs">
      <Check aria-hidden="true" className="size-3" />
      Current
    </span>
  );
}
