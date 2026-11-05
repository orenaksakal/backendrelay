import { createPortal } from 'react-dom';
import {
  OriginalAppPicker,
  OriginalNavigation,
} from '@/components/dash/OriginalNavigation';

import {
  connectionMatches,
  emptyWorkspaceDestination,
} from '@/components/dash/shell/model';
import {
  ArchiveBoxIcon,
  BeakerIcon,
  CodeBracketIcon,
  CubeIcon,
  FunnelIcon,
  HomeIcon,
  IdentificationIcon,
  LockClosedIcon,
  MagnifyingGlassIcon,
  ShieldCheckIcon,
} from '@heroicons/react/24/outline';

import { init } from '@instantdb/react';
import produce from 'immer';
import Link from '@/lib/navigation';
import Head from '@/components/PageHead';
import { useContext, useEffect, useMemo, useRef, useState } from 'react';
import { usePostHog } from 'posthog-js/react';

import config, { cliOauthParamName, isSelfHosted } from '@/lib/config';
import { TokenContext } from '@/lib/contexts';
import { jsonFetch, jsonMutate } from '@/lib/fetch';
import { successToast } from '@/lib/toast';
import { InstantApp } from '@/lib/types';

import { asClientOnlyPage, useReadyRouter } from '@/components/clientOnlyPage';

import { useFetchedDash } from '@/components/dash/MainDashLayout';

import WebhookIcon from '@/components/icons/WebhookIcon';
import { Badge, SmallCopyable, TabItem, twel } from '@/components/ui';
import { SearchFilter, useSchemaQuery } from '@/lib/hooks/explorer';
import useLocalStorage from '@/lib/hooks/useLocalStorage';
import { getLocallySavedApp, setLocallySavedApp } from '@/lib/locallySavedApp';
import { capitalize } from 'lodash';
import { Workspace } from '@/lib/hooks/useWorkspace';
import { useExplorerState } from '@/lib/hooks/useExplorerState';
import { displayAppStatus } from '@/lib/appStatus';
import { OriginalHome } from '@/components/dash/OriginalHome';
import { useDarkMode } from '@/components/dash/DarkModeToggle';
import { explorerStorageHandlers } from '@/components/dash/explorer/storage';

import { lazyDashboard } from '@/components/dash/LazyDashboard';
const Explorer = lazyDashboard(() =>
  import('@instantdb/components').then((module) => ({
    default: module.Explorer,
  })),
);
const Perms = lazyDashboard(() =>
  import('@/components/dash/Perms').then((module) => ({
    default: module.Perms,
  })),
);
const Schema = lazyDashboard(() =>
  import('@/components/dash/Schema').then((module) => ({
    default: module.Schema,
  })),
);
const Admin = lazyDashboard(() =>
  import('@/components/admin/AdminPage').then((module) => ({
    default: module.Admin,
  })),
);
const AppAuth = lazyDashboard(() =>
  import('@/components/dash/AppAuth').then((module) => ({
    default: module.AppAuth,
  })),
);
const QueryInspector = lazyDashboard(() =>
  import('@/components/dash/explorer/QueryInspector').then((module) => ({
    default: module.QueryInspector,
  })),
);
const Backups = lazyDashboard(() =>
  import('@/components/dash/Backups').then((module) => ({
    default: module.Backups,
  })),
);
const Sandbox = lazyDashboard(() =>
  import('@/components/dash/Sandbox').then((module) => ({
    default: module.Sandbox,
  })),
);
const Webhooks = lazyDashboard(() =>
  import('@/components/dash/Webhooks').then((module) => ({
    default: module.Webhooks,
  })),
);
const OAuthApps = lazyDashboard(() => import('@/components/dash/OAuthApps'));

// (XXX): we may want to expose this underlying type
type InstantReactClient = ReturnType<typeof init>;

export type Role = 'collaborator' | 'admin' | 'owner' | 'app-member';

export const roleOrder = [
  'app-member',
  'collaborator',
  'admin',
  'owner',
] as const;

type MainTabId =
  | 'home'
  | 'explorer'
  | 'schema'
  | 'repl'
  | 'sandbox'
  | 'perms'
  | 'auth'
  | 'webhooks'
  | 'backups'
  | 'email'
  | 'team'
  | 'admin'
  | 'oauth-apps';

type UserSettingsTabId = 'pat' | 'oauth-apps';

type Screen =
  | 'main' // app details
  | 'user-settings'
  | 'invites'
  | 'new'
  | 'personal-access-tokens';

const defaultTab: MainTabId | UserSettingsTabId = 'home';

interface Tab<TabId> {
  id: TabId;
  title: string;
  icon?: React.ReactNode;
  minRole?: 'collaborator' | 'admin' | 'owner';
}

const makeIcon = (Icon: typeof HomeIcon) => {
  return <Icon width={14} />;
};

const mainTabs: Tab<MainTabId>[] = [
  { id: 'home', title: 'Home', icon: makeIcon(HomeIcon) },
  { id: 'explorer', title: 'Explorer', icon: makeIcon(FunnelIcon) },
  { id: 'schema', title: 'Schema', icon: makeIcon(CodeBracketIcon) },
  { id: 'perms', title: 'Permissions', icon: makeIcon(LockClosedIcon) },
  { id: 'auth', title: 'Auth', icon: makeIcon(IdentificationIcon) },
  { id: 'webhooks', title: 'Webhooks', icon: makeIcon(WebhookIcon) },
  { id: 'backups', title: 'Backups', icon: makeIcon(ArchiveBoxIcon) },
  { id: 'repl', title: 'Query Inspector', icon: makeIcon(MagnifyingGlassIcon) },
  { id: 'sandbox', title: 'Sandbox', icon: makeIcon(BeakerIcon) },
  {
    id: 'admin',
    title: 'Admin',
    minRole: 'admin',
    icon: makeIcon(ShieldCheckIcon),
  },
  { id: 'oauth-apps', title: 'OAuth Apps', icon: makeIcon(CubeIcon) },
];

const userTabs: Tab<UserSettingsTabId>[] = [
  { id: 'oauth-apps', title: 'OAuth Apps' },
  { id: 'pat', title: 'Access Tokens' },
];

const mainTabIndex = new Map(mainTabs.map((t) => [t.id, t]));

export function isMinRole(minRole: Role, role: Role) {
  return roleOrder.indexOf(role) >= roleOrder.indexOf(minRole);
}

// COMPONENTS

const Dash = asClientOnlyPage(DashV2);

export default Dash;

function DashV2() {
  return <Dashboard key="root" />;
}

function dashHref(query: Record<string, string>) {
  return `/dash?${new URLSearchParams(query).toString()}`;
}

function isTabAvailable(tab: Tab<MainTabId>, role: Role) {
  return tab.minRole ? role && isMinRole(tab.minRole, role) : true;
}

function screenTab(screen: Screen, tab: string | null | undefined) {
  return tab && mainTabIndex.has(tab as MainTabId) ? tab : defaultTab;
}

const getInitialApp = (apps: InstantApp[], workspaceId: string) => {
  const firstApp = apps?.[0];
  if (!firstApp) return;

  const lastApp = getLocallySavedApp(workspaceId);
  const lastAppId =
    lastApp && Boolean(apps.find((a) => a.id === lastApp.id))
      ? lastApp.id
      : null;

  const defaultAppId = lastAppId ?? firstApp.id;
  if (!defaultAppId) return;
  return defaultAppId;
};

const roleIndexed: Role[] = ['collaborator', 'admin', 'owner'];

export const getRole = (
  dataFromDash: ReturnType<typeof useFetchedDash>['data'],
  app: InstantApp,
): Role => {
  if (dataFromDash.workspace.type === 'personal') {
    return app.user_app_role ?? 'collaborator';
  } else {
    if (dataFromDash.workspace.org.role === 'app-member') {
      return app.user_app_role ?? 'app-member';
    }
    // return the max between the two roles
    return roleIndexed.reduce((max, role) => {
      if (dataFromDash.workspace.type === 'personal') {
        return app.user_app_role ?? 'collaborator'; // should never hit this
      }
      if (dataFromDash.workspace.org.role === role) return role;
      if (app.user_app_role === role) return role;
      return max;
    }, roleIndexed[0]);
  }
};

function Dashboard() {
  const token = useContext(TokenContext);
  const router = useReadyRouter();
  const fetchedDash = useFetchedDash();
  const posthog = usePostHog();
  const apps = fetchedDash.data.apps;

  const appId =
    (router.query.app as string) ||
    getInitialApp(apps, fetchedDash.data.currentWorkspaceId);

  const screen = ((router.query.s as string) || 'main') as Screen;
  const tab = screenTab(screen, router.query.t as string);
  const viewRef = useRef<HTMLDivElement>(null);
  const lastView = useRef(`${appId}:${tab}`);

  const dashResponse = useFetchedDash();
  const cliNormalTicket = router.query.ticket as string | undefined;
  const cliOauthTicket = router.query[cliOauthParamName] as string | undefined;

  const cliTicketQuery = {
    ...(cliNormalTicket ? { ticket: cliNormalTicket } : {}),
    ...(cliOauthTicket ? { [cliOauthParamName]: cliOauthTicket } : {}),
  };

  // Local states
  const [hideAppId, setHideAppId] = useLocalStorage('hide_app_id', false);

  const [connection, setConnection] = useState<{
    db: InstantReactClient;
    appId: string;
    adminToken: string;
  } | null>(null);
  const [appLookupError, setAppLookupError] = useState('');
  const [lookupRevision, setLookupRevision] = useState(0);

  const [agentEssayDemo, setAgentEssayDemo] = useLocalStorage<{
    appId?: string;
    adminToken?: string;
    claimed?: boolean;
  }>('agents-essay-demo', {});

  // backwards compatible routing
  useEffect(() => {
    if (screen === 'new') {
      router.replace('/dash/new');
      return;
    }
    if (screen === 'invites') {
      router.replace('/dash/user-settings?tab=invites');
      return;
    }
    if (screen === 'user-settings' || screen === 'personal-access-tokens') {
      if (tab === 'oauth-apps') {
        router.replace('/dash/user-settings?tab=oauth');
        return;
      }
      router.replace('/dash/user-settings');
    }
  }, []);

  useEffect(() => {
    if (!token) return;
    if (agentEssayDemo.claimed) return;
    if (!agentEssayDemo.appId || !agentEssayDemo.adminToken) return;

    jsonMutate(
      `${config.apiURI}/dash/apps/ephemeral/${agentEssayDemo.appId}/claim`,
      {
        token,
        method: 'POST',
        body: { token: agentEssayDemo.adminToken },
      },
    ).then(() => {
      setAgentEssayDemo({ ...agentEssayDemo, claimed: true });
    });
  }, [token, agentEssayDemo]);

  const app = apps?.find((a) => a.id === appId);

  // ui
  const showApp =
    app &&
    connection &&
    connectionMatches(connection, app.id, app.admin_token) &&
    screen === 'main';

  useEffect(() => {
    if (!showApp || !viewRef.current) return;
    viewRef.current.scrollTo({ top: 0 });
    if (lastView.current !== `${appId}:${tab}`) {
      viewRef.current.focus({ preventScroll: true });
      lastView.current = `${appId}:${tab}`;
    }
  }, [tab, appId, showApp]);

  // set the query params if there are none
  useEffect(() => {
    if (!app) return;
    if (!router.query.app || !router.query.t) {
      router.replace(
        dashHref({
          ...cliTicketQuery,
          s: 'main',
          app: app.id,
          t: tab,
          ...(dashResponse.data.currentWorkspaceId === 'personal'
            ? {}
            : { org: dashResponse.data.currentWorkspaceId }),
        }),
      );
    }
  }, [
    app,
    dashResponse.data.currentWorkspaceId,
    router.query.app,
    router.query.t,
    tab,
  ]);

  useEffect(() => {
    if (screen && screen !== 'main') return;
    setAppLookupError('');

    const isAppIdValid = Boolean(apps.find((a) => a.id === appId));
    if (appId && isAppIdValid) return;

    const lastApp = getLocallySavedApp(dashResponse.data.currentWorkspaceId);

    const lastAppId =
      lastApp && Boolean(apps.find((a) => a.id === lastApp.id))
        ? lastApp.id
        : null;

    const firstApp = apps?.[0];

    const defaultAppId = lastAppId ?? firstApp?.id;

    const replaceDefault = () => {
      if (!defaultAppId) return;

      router.replace(
        dashHref({
          ...cliTicketQuery,
          s: 'main',
          app: defaultAppId,
          t: tab,
          ...(dashResponse.data.currentWorkspaceId === 'personal'
            ? {}
            : { org: dashResponse.data.currentWorkspaceId }),
        }),
      );

      setLocallySavedApp({
        id: defaultAppId,
        orgId: dashResponse.data.currentWorkspaceId,
      });
    };

    if (appId && appId !== lastAppId) {
      let cancel = false;
      let timedOut = false;
      const abort = new AbortController();
      const timer = window.setTimeout(() => {
        timedOut = true;
        abort.abort();
      }, 15000);
      // If we didn't find the app, check if the app lives on
      // a different org and redirect to that.
      jsonFetch(`${config.apiURI}/dash/apps/${encodeURIComponent(appId)}`, {
        signal: abort.signal,
        method: 'GET',
        headers: { Authorization: `Bearer ${token}` },
      })
        .then((res) => {
          if (cancel) return;
          if (
            res?.app?.creator_id &&
            dashResponse.data?.currentWorkspaceId &&
            dashResponse.data?.currentWorkspaceId !== 'personal'
          ) {
            dashResponse.setWorkspace('personal');
          } else if (
            res?.app?.org_id &&
            res?.app?.org_id !== router.query.org
          ) {
            dashResponse.setWorkspace(res.app.org_id);
            router.replace(
              dashHref({
                ...cliTicketQuery,
                s: 'main',
                app: appId,
                org: res.app.org_id,
                t: tab,
              }),
            );
          } else {
            setAppLookupError(
              'The requested application is not in this workspace’s application list. Refresh the list or open the workspace without this link.',
            );
          }
        })
        .catch((e) => {
          if (!cancel)
            setAppLookupError(
              timedOut
                ? 'Application lookup timed out. Try again.'
                : e?.body?.message ||
                    e?.message ||
                    'Could not load the requested application. Try again.',
            );
        })
        .finally(() => window.clearTimeout(timer));

      return () => {
        cancel = true;
        window.clearTimeout(timer);
        abort.abort();
      };
    }

    replaceDefault();
  }, [
    dashResponse.data?.currentWorkspaceId,
    appId,
    router.query.org,
    lookupRevision,
    apps,
  ]);

  useEffect(() => {
    if (!app) return;
    if (typeof window === 'undefined') return;

    const db = init({
      appId: app.id,
      apiURI: config.apiURI,
      websocketURI: config.websocketURI,
      // @ts-expect-error
      __adminToken: app?.admin_token,
      disableValidation: true,
    });

    setConnection({ db, appId: app.id, adminToken: app.admin_token });
    return () => {
      db.core.shutdown();
    };
  }, [app?.id, app?.admin_token]);

  function nav(
    q: { s: string; app?: string; t?: string },
    opts?: { cb?: () => void; trackClick?: boolean },
  ) {
    if (q.app)
      setLocallySavedApp({
        id: q.app,
        orgId: dashResponse.data.currentWorkspaceId,
      });

    // Track tab navigation only for user-initiated clicks
    if (q.t && opts?.trackClick) {
      posthog.capture('dashboard_tab_click', {
        tab: q.t,
        app_id: q.app || appId,
      });
    }

    Promise.resolve(
      router.push({
        query: {
          ...cliTicketQuery,
          ...q,
          ...(dashResponse.data.currentWorkspaceId === 'personal'
            ? {}
            : { org: dashResponse.data.currentWorkspaceId }),
        },
      }),
    ).then(() => {
      if (opts?.cb) {
        opts.cb();
      }
    });
  }

  async function onDeleteApp(app: InstantApp) {
    successToast(
      `${app.title} is marked for deletion. We will remove all data in 24 hours. ${
        isSelfHosted
          ? 'Contact your deployment administrator if you did not mean to do this.'
          : 'Ping us on Discord if you did not mean to do this.'
      }`,
    );
    const _apps = apps.filter((a) => a.id !== app.id);
    if (dashResponse.data.workspace.type === 'personal') {
      dashResponse.mutate((data) =>
        produce(data, (d) => {
          if (d) {
            d.apps = _apps;
          }
        }),
      );
    } else {
      await dashResponse.refetch();
    }
    const _appId = _apps[0]?.id;
    nav({ s: 'main', app: _appId, t: 'hello' });
  }

  const emptyDestination =
    appId && !app
      ? null
      : emptyWorkspaceDestination(
          apps.length,
          dashResponse.data.invites?.length ?? 0,
          dashResponse.data.orgs?.length ?? 0,
        );
  useEffect(() => {
    if (!emptyDestination) return;
    router.replace(
      {
        pathname: emptyDestination,
        query: {
          ...cliTicketQuery,
          ...(emptyDestination === '/dash/user-settings'
            ? { tab: 'invites' }
            : {}),
          ...(dashResponse.data.currentWorkspaceId !== 'personal'
            ? { org: dashResponse.data.currentWorkspaceId }
            : {}),
        },
      },
      undefined,
      { shallow: true },
    );
  }, [
    emptyDestination,
    dashResponse.data.currentWorkspaceId,
    cliNormalTicket,
    cliOauthTicket,
  ]);
  if (emptyDestination) return null;

  if (!appId || !app) {
    return (
      <section className="shell-load-error">
        <p className="dashboard-resource-eyebrow">Application</p>
        <h1>
          {appLookupError ? 'Application unavailable' : 'Finding application…'}
        </h1>
        {appId && <code>{appId}</code>}
        {appLookupError && (
          <>
            <p role="alert">{appLookupError}</p>
            <div className="wb-actions">
              <button
                onClick={async () => {
                  setAppLookupError('');
                  try {
                    await dashResponse.refetch();
                  } catch (e) {
                    setAppLookupError(
                      e instanceof Error ? e.message : 'Refresh failed.',
                    );
                  }
                  setLookupRevision((v) => v + 1);
                }}
              >
                Retry application lookup
              </button>
              <button onClick={() => nav({ s: 'main', t: 'home' })}>
                Open workspace
              </button>
            </div>
          </>
        )}
      </section>
    );
  }

  // Role is the max between the org and the app
  const role = getRole(dashResponse.data, app);
  const appStatus = displayAppStatus(app);
  const availableTabs: TabItem[] = mainTabs
    .filter((t) => isTabAvailable(t, role))
    .map((t) => {
      return {
        id: t.id,
        label: t.title,
        icon: t.icon,
      };
    });

  return (
    <>
      <div className="flex min-h-0 flex-1 flex-col">
        <Head>
          <title>
            BackendRelay - {mainTabIndex.get(tab as MainTabId)?.title}
          </title>
        </Head>
        <div className="flex shrink-0 flex-col justify-between border-b border-b-gray-300 bg-gray-50 px-3 py-2 md:flex-row md:gap-4 dark:border-b-neutral-700 dark:bg-neutral-800/90">
          <div className="flex min-w-0 items-center gap-2">
            <h2 className="truncate font-semibold md:text-xl">{app.title}</h2>
            {appStatus === 'read-only' && (
              <Badge className="bg-amber-100 text-amber-800 dark:bg-amber-500/20 dark:text-amber-200">
                Read-only
              </Badge>
            )}
            {appStatus === 'disabled' && (
              <Badge className="bg-red-100 text-red-800 dark:bg-red-500/20 dark:text-red-200">
                Disabled
              </Badge>
            )}
            {dashResponse.data.workspace.type === 'org' && (
              <Badge>{capitalize(dashResponse.data.workspace.org.role)}</Badge>
            )}
          </div>
          <SmallCopyable
            size="normal"
            label="Public App ID"
            value={app.id}
            hideValue={hideAppId}
            onChangeHideValue={() => {
              setHideAppId(!hideAppId);
            }}
          />
        </div>
        <div className="flex min-h-0 flex-1 flex-col md:flex-row">
          <Nav
            apps={apps}
            appId={appId}
            tab={tab as MainTabId}
            availableTabs={availableTabs}
            nav={(params, opts) =>
              nav({ s: 'main', app: appId, ...params }, opts)
            }
            screen={screen}
          />
          {showApp ? (
            <div
              key={appId}
              ref={viewRef}
              className="dashboard-view flex min-h-0 min-w-0 flex-1 flex-col overflow-auto"
              id="dashboard-page-content"
              role="region"
              aria-label={
                mainTabIndex.get(tab as MainTabId)?.title ??
                'Application workspace'
              }
              tabIndex={-1}
            >
              <DashboardContent
                role={role}
                connection={connection}
                app={app}
                appId={appId}
                tab={tab}
                nav={nav}
                onDeleteApp={onDeleteApp}
                workspace={dashResponse.data.workspace}
              />
            </div>
          ) : (
            <div
              className="dashboard-view flex-1 p-8 text-gray-500"
              role="status"
            >
              Connecting to application…
            </div>
          )}
        </div>
      </div>
    </>
  );
}

export const TabContent = twel('div', 'flex flex-col max-w-2xl gap-4 p-4');

function mergeQueryParams(query: string) {
  const newQuery = new URLSearchParams(query);
  const currentQuery = new URLSearchParams(window.location.search);
  newQuery.forEach((value, key) => {
    currentQuery.set(key, value);
  });
  return Object.fromEntries(currentQuery);
}

// When navigating dash routes we want to persist query params containing
// things like the app id
function formatDashRoute(href: string) {
  const root = '/dash';
  if (!href.startsWith(root)) {
    return href;
  }

  const [pathName, queryString = ''] = href.split('?');
  const mergedQueryParams = mergeQueryParams(queryString);
  const mergedQueryString = Object.entries(mergedQueryParams)
    .map(
      ([key, value]) =>
        `${encodeURIComponent(key)}=${encodeURIComponent(value)}`,
    )
    .join('&');

  return `${pathName}?${mergedQueryString}`;
}

function formatDocsRoute(href: string) {
  const root = '/docs';

  if (!href.startsWith(root)) {
    return href;
  }

  const { app: appId } = Object.fromEntries(
    new URLSearchParams(window.location.search),
  );

  if (!appId) {
    return href;
  }

  const [pathName, hash] = href.split('#');

  if (hash) {
    return `${pathName}?app=${appId}#${hash}`;
  } else {
    return `${pathName}?app=${appId}`;
  }
}

function formatRouteParams(href: string) {
  return formatDashRoute(formatDocsRoute(href));
}

// Retained for settings pages that share dashboard shortcut cards.
export function HomeButton({
  href,
  title,
  children,
  onClick,
  target,
}: {
  href: string;
  title: string;
  children: React.ReactNode;
  onClick?: () => void;
  target?: string;
}) {
  return (
    <Link
      href={formatRouteParams(href)}
      target={target}
      rel={target === '_blank' ? 'noopener noreferrer' : undefined}
      className="block cursor-pointer justify-start space-y-2 rounded-sm border bg-white p-4 shadow-xs transition-colors hover:bg-gray-50 disabled:text-gray-400 dark:border-neutral-700 dark:bg-neutral-800 dark:hover:bg-neutral-700/50"
      onClick={onClick}
    >
      <div className="font-bold">{title}</div>
      <div className="text-sm text-gray-500 dark:text-neutral-400">
        {children}
      </div>
    </Link>
  );
}

// Dashboard content component that manages schema subscription
function DashboardContent({
  connection,
  app,
  appId,
  tab,
  nav,
  role,
  onDeleteApp,
  workspace,
}: {
  connection: { db: InstantReactClient };
  app: InstantApp;
  appId: string;
  tab: string;
  role: Role;
  nav: (
    q: { s: string; app?: string; t?: string },
    opts?: { cb?: () => void; trackClick?: boolean },
  ) => void;
  onDeleteApp: (app: InstantApp) => void;
  workspace: Workspace;
}) {
  // Subscribe to schema changes at the dashboard level
  const schemaData = useSchemaQuery(connection.db);
  const token = useContext(TokenContext)!;
  const destination = mainTabIndex.get(tab as MainTabId);
  if (destination && !isTabAvailable(destination, role))
    return (
      <section className="shell-load-error">
        <h1>This page is unavailable for your role</h1>
        <p>
          Your current role is {role}. Choose another page or contact an
          application administrator.
        </p>
        <button onClick={() => nav({ s: 'main', app: appId, t: 'home' })}>
          Open overview
        </button>
      </section>
    );

  return (
    <>
      {tab === 'home' ? (
        <OriginalHome app={app} token={token} />
      ) : tab === 'explorer' ? (
        <ExplorerTab key={appId} appId={appId} db={connection.db} />
      ) : tab === 'schema' ? (
        <Schema key={appId} appId={appId} attrs={schemaData.attrs} />
      ) : tab === 'repl' ? (
        <QueryInspector
          className="w-full flex-1"
          appId={appId}
          db={connection.db}
          namespaces={schemaData.namespaces}
          attrs={schemaData.attrs}
        />
      ) : tab === 'sandbox' ? (
        <Sandbox
          key={appId}
          app={app}
          db={connection.db}
          attrs={schemaData.attrs}
          namespaces={schemaData.namespaces}
        />
      ) : tab === 'perms' ? (
        <Perms
          app={app}
          db={connection.db}
          namespaces={schemaData.namespaces}
        />
      ) : tab === 'auth' ? (
        <AppAuth app={app} key={app.id} nav={nav} />
      ) : tab === 'webhooks' ? (
        <Webhooks app={app} namespaces={schemaData.namespaces} />
      ) : tab === 'backups' ? (
        <Backups app={app} />
      ) : tab === 'admin' && isMinRole('admin', role) ? (
        <Admin
          role={role}
          app={app}
          onDelete={() => onDeleteApp(app)}
          nav={nav}
          workspace={workspace}
        />
      ) : tab === 'oauth-apps' ? (
        <OAuthApps appId={appId} />
      ) : null}
    </>
  );
}

export function AppCombobox({
  workspaceId,
  apps,
  appId,
  nav,
  tab,
}: {
  workspaceId: string;
  apps: InstantApp[];
  nav: (
    p: { s: string; t?: string; app?: string },
    opts?: { cb?: () => void; trackClick?: boolean },
  ) => void;
  appId: string;
  tab: MainTabId;
}) {
  return (
    <OriginalAppPicker
      key={workspaceId}
      workspaceId={workspaceId}
      apps={apps}
      appId={appId}
      onSelect={(app) => nav({ s: 'main', app: app.id, t: tab })}
    />
  );
}

function Nav({
  apps,
  nav,
  appId,
  tab,
  availableTabs,
}: {
  apps?: InstantApp[];
  nav: (
    p: { s?: string; t?: string; app?: string },
    opts?: { cb?: () => void; trackClick?: boolean },
  ) => void;
  appId: string;
  tab: MainTabId | UserSettingsTabId;
  availableTabs: TabItem[];
  screen: string;
}) {
  const {
    data: { currentWorkspaceId },
  } = useFetchedDash();
  return (
    <>
      {apps &&
        document.getElementById('left-top-bar') &&
        createPortal(
          <AppCombobox
            workspaceId={currentWorkspaceId}
            apps={apps}
            appId={appId}
            nav={nav}
            tab={tab as MainTabId}
          />,
          document.getElementById('left-top-bar')!,
        )}
      <OriginalNavigation
        tab={tab}
        items={availableTabs}
        href={(t) =>
          dashHref({
            s: 'main',
            app: appId,
            t,
            ...(currentWorkspaceId === 'personal'
              ? {}
              : { org: currentWorkspaceId }),
          })
        }
        onNavigate={(t) => nav({ t }, { trackClick: true })}
      />
    </>
  );
}

function ExplorerTab({ db, appId }: { db: InstantReactClient; appId: string }) {
  const { darkMode } = useDarkMode();
  const token = useContext(TokenContext);
  const [explorerState, setExplorerState] = useExplorerState();
  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-hidden">
      <Explorer
        useShadowDOM={false}
        setExplorerState={setExplorerState}
        explorerState={explorerState}
        apiURI={config.apiURI}
        websocketURI={config.websocketURI}
        darkMode={darkMode}
        appId={appId}
        adminToken={db.core._reactor.config.__adminToken}
        {...explorerStorageHandlers(config.apiURI, appId, token)}
      />
    </div>
  );
}

export function FullscreenErrorMessage({ message }: { message: string }) {
  return (
    <div className="mx-auto flex w-full max-w-2xl flex-col gap-4 p-2">
      <div className="rounded-sm bg-red-100 p-4 text-red-700">{message}</div>
    </div>
  );
}
