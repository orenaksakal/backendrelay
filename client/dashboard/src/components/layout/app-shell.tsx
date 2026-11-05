import {
  Outlet,
  useRouter,
  useRouterState,
  type NavigateOptions,
} from '@tanstack/react-router';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  CommandPalette,
  type PaletteMode,
} from '@/components/layout/command-palette';
import { ShortcutsSheet } from '@/components/layout/shortcuts-sheet';
import { Sidebar, SidebarDrawer } from '@/components/layout/sidebar';
import { TopBar } from '@/components/layout/top-bar';
import { Toaster, ToastProvider } from '@/components/ui/toast';
import { TooltipProvider } from '@/components/ui/tooltip';
import { toastManager } from '@/lib/toast';
import {
  accountNavigation,
  activeNavItem,
  appNavigation,
  documentTitle,
  operatorNavigation,
  pageLabelFromPath,
  workspaceIdFromPath,
  workspaceNavigation,
  type Navigation,
} from '@/lib/navigation';
import {
  PAGE_ACTION_EVENT,
  useGlobalShortcuts,
  type ShortcutContext,
} from '@/lib/shortcuts';
import { useSidebarState } from '@/lib/sidebar';
import { useAppRouteContext } from '@/lib/workspace/app-context';
import { listWorkspaces } from '@/lib/workspace/workspaces';
import { useBootstrap } from '@/lib/workspace/queries';

/**
 * Signed-in shell: skip link, 48px top bar, per-area sidebar and the `main`
 * landmark. Focus moves to the new page's `h1` after navigation and the
 * document title follows `{Page} · {App} · BackendRelay` (D3.4). The command
 * palette and the `?` sheet are mounted here so the global key handler can
 * reach them from every screen (D3.5).
 */
export function AppShell() {
  const router = useRouter();
  const pathname = useRouterState({
    select: (state) => state.location.pathname,
  });
  const appContext = useAppRouteContext();
  const bootstrap = useBootstrap();
  const [sidebarState, setSidebarState] = useSidebarState();
  const [drawerOpen, setDrawerOpen] = useState(false);
  const [paletteMode, setPaletteMode] = useState<PaletteMode | null>(null);
  const [shortcutsOpen, setShortcutsOpen] = useState(false);

  const workspaces = useMemo(
    () => listWorkspaces(bootstrap.data),
    [bootstrap.data],
  );
  const workspaceId =
    appContext?.workspaceId ?? workspaceIdFromPath(pathname) ?? null;
  const workspace = workspaces.find(
    (candidate) => candidate.id === workspaceId,
  );
  const loaderApp = appContext?.app;
  // The bootstrap copy is live: a rename on the settings screen updates the
  // breadcrumb and document title without waiting for a loader refresh.
  const bootstrapApp =
    loaderApp === undefined
      ? undefined
      : bootstrap.data?.apps.find((candidate) => candidate.id === loaderApp.id);
  const app = bootstrapApp ?? loaderApp;

  const navigation = useMemo<Navigation | null>(() => {
    if (appContext !== undefined) return appNavigation(appContext.app.id);
    if (workspaceId !== null) {
      return workspaceNavigation({
        workspaceId,
        isOrg: workspace?.kind === 'org',
      });
    }
    if (pathname === '/account' || pathname.startsWith('/account/')) {
      return accountNavigation();
    }
    if (pathname.startsWith('/operator/')) return operatorNavigation();
    return null;
  }, [appContext, pathname, workspace?.kind, workspaceId]);

  const active = navigation ? activeNavItem(navigation, pathname) : undefined;
  const pageLabel = active?.label ?? pageLabelFromPath(pathname);
  const appTitle = app?.title;

  useEffect(() => {
    document.title = documentTitle(pageLabel, appTitle);
  }, [appTitle, pageLabel]);

  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect -- navigation closes the mobile drawer
    setDrawerOpen(false);
  }, [pathname]);

  // The first render is the page's own load; only later navigations move focus.
  const firstRender = useRef(true);
  useEffect(() => {
    if (firstRender.current) {
      firstRender.current = false;
      return;
    }
    const timer = window.setTimeout(() => {
      // Never steal focus from a field the user already reached.
      const active = document.activeElement;
      if (
        active instanceof HTMLElement &&
        active.closest(
          'input, textarea, select, [contenteditable]:not([contenteditable="false"]), .cm-editor',
        ) !== null
      ) {
        return;
      }
      document.getElementById('page-title')?.focus({ preventScroll: true });
    }, 0);
    return () => {
      window.clearTimeout(timer);
    };
  }, [pathname]);

  const toggleSidebar = useCallback(() => {
    setSidebarState(sidebarState === 'collapsed' ? 'expanded' : 'collapsed');
  }, [setSidebarState, sidebarState]);

  const shortcutContext = useMemo<ShortcutContext>(
    () => ({
      navigate: (href) => {
        void router.navigate({ href } as NavigateOptions);
      },
      openPalette: () => {
        setShortcutsOpen(false);
        setPaletteMode('all');
      },
      openShortcutsSheet: () => {
        setPaletteMode(null);
        setShortcutsOpen(true);
      },
      toggleSidebar,
      // Esc closes one layer at a time; dialogs also handle their own Esc.
      closeTopLayer: () => {
        if (paletteMode !== null) {
          setPaletteMode(null);
          return;
        }
        if (shortcutsOpen) {
          setShortcutsOpen(false);
          return;
        }
        setDrawerOpen(false);
      },
      requestPageAction: (action) => {
        // A screen answers by listening and calling preventDefault (D6+).
        const handled = !window.dispatchEvent(
          new CustomEvent(PAGE_ACTION_EVENT, {
            detail: { action },
            cancelable: true,
          }),
        );
        // `/` opens the palette when the page has no search field (B.7).
        if (!handled && action === 'search') {
          setShortcutsOpen(false);
          setPaletteMode('all');
        }
      },
      appId: app?.id,
      workspaceId: workspaceId ?? undefined,
    }),
    [app?.id, paletteMode, router, shortcutsOpen, toggleSidebar, workspaceId],
  );

  useGlobalShortcuts(shortcutContext);

  return (
    <ToastProvider toastManager={toastManager}>
      <TooltipProvider>
        <div className="bg-background flex min-h-dvh flex-col">
          <a
            href="#main"
            className="bg-card text-foreground sr-only rounded-md px-3 py-2 shadow-md focus:not-sr-only focus:absolute focus:top-2 focus:left-2 focus:z-(--z-command)"
          >
            Skip to content
          </a>
          <TopBar
            navigation={navigation}
            pathname={pathname}
            app={app}
            workspace={workspace}
            superuser={bootstrap.data?.superuser === true}
            menuOpen={drawerOpen}
            onOpenMenu={() => {
              setDrawerOpen(true);
            }}
            onOpenPalette={setPaletteMode}
            onOpenShortcuts={() => {
              setShortcutsOpen(true);
            }}
          />
          <div className="flex min-h-0 flex-1">
            {navigation === null ? null : (
              <Sidebar
                navigation={navigation}
                pathname={pathname}
                state={sidebarState}
                onToggle={toggleSidebar}
              />
            )}
            <main id="main" tabIndex={-1} className="min-w-0 flex-1 p-4 md:p-6">
              <Outlet />
            </main>
          </div>
          {navigation === null ? null : (
            <SidebarDrawer
              open={drawerOpen}
              onOpenChange={setDrawerOpen}
              navigation={navigation}
              pathname={pathname}
            />
          )}
        </div>
        <CommandPalette
          open={paletteMode !== null}
          onOpenChange={(open) => {
            if (!open) setPaletteMode(null);
          }}
          mode={paletteMode ?? 'all'}
          app={app}
          sectionId={active?.id}
          workspaceId={workspaceId ?? undefined}
          workspaces={workspaces}
          apps={bootstrap.data?.apps ?? []}
          superuser={bootstrap.data?.superuser === true}
          onToggleSidebar={toggleSidebar}
        />
        <ShortcutsSheet open={shortcutsOpen} onOpenChange={setShortcutsOpen} />
        <Toaster />
      </TooltipProvider>
    </ToastProvider>
  );
}
