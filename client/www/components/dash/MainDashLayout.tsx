import { useDashFetch } from '@/lib/hooks/useDashFetch';
import PageHead from '@/components/PageHead';
import { useRouter } from '@/lib/navigation';
import { ReactNode, useCallback, useEffect, useState } from 'react';
import { cn, FullscreenLoading } from '../ui';
import { useAuthToken } from '@/lib/auth';
import Auth from './Auth';
import { TokenContext } from '@/lib/contexts';
import { CLILoginDialog } from './CLILoginDialog';
import { useTicketSystem } from '@/lib/hooks/useTicketSystem';
import { createInitializedContext } from '@/lib/createInitializedContext';
import { TopBar } from './TopBar';
import { useWorkspace } from '@/lib/hooks/useWorkspace';
import {
  initialWorkspace,
  resolveWorkspace,
  singleQuery,
  type WorkspaceSelection,
} from './shell/model';
import { useReadyRouter } from '../clientOnlyPage';
import { useDarkMode } from './DarkModeToggle';
import { Toaster } from '@instantdb/components';
import '@/styles/original-dashboard.css';

export type FetchedDash = ReturnType<typeof useFetchedDash>;

const savedWorkspace = () => {
  try {
    return typeof window === 'undefined'
      ? null
      : window.localStorage.getItem('workspace');
  } catch {
    return null;
  }
};

export const { use: useFetchedDash, provider: DashFetchProvider } =
  createInitializedContext(
    'dashResponse',
    (args?: { workspaceId?: string | null | undefined }) => {
      const dashResult = useDashFetch();
      const router = useReadyRouter();
      const urlOrg = singleQuery(router.query.org);
      const [selection, setSelection] = useState<WorkspaceSelection>(() => ({
        id: args?.workspaceId || initialWorkspace(urlOrg, savedWorkspace()),
        urlOrg,
      }));
      const incomingUrl = selection.urlOrg !== urlOrg;
      const currentWorkspaceId = resolveWorkspace(selection, urlOrg);
      const setWorkspace = useCallback(
        (id: string) => {
          setSelection({ id, urlOrg });
        },
        [urlOrg],
      );
      useEffect(() => {
        if (incomingUrl) setSelection({ id: currentWorkspaceId, urlOrg });
      }, [incomingUrl, currentWorkspaceId, urlOrg]);

      const workspace = useWorkspace(dashResult, currentWorkspaceId);
      const optimisticUpdateCurrentWorkspace =
        currentWorkspaceId === 'personal'
          ? dashResult.optimisticUpdate
          : workspace.optimisticUpdate;

      const refetch = async () => {
        await dashResult.mutate();
        await workspace.mutate();
      };

      useEffect(() => {
        if (typeof window === 'undefined' || incomingUrl) return;
        try {
          window.localStorage.setItem('workspace', currentWorkspaceId);
        } catch {}
        const currentUrl = new URL(window.location.href);
        const expected =
          currentWorkspaceId === 'personal' ? null : currentWorkspaceId;
        if (currentUrl.searchParams.get('org') === expected) return;
        if (expected) currentUrl.searchParams.set('org', expected);
        else currentUrl.searchParams.delete('org');
        router.replace(
          currentUrl.pathname + currentUrl.search + currentUrl.hash,
          undefined,
          { shallow: true },
        );
      }, [currentWorkspaceId, incomingUrl, urlOrg, router.pathname]);

      return {
        ready: !!dashResult.data && !!workspace.data,
        refetch,
        setWorkspace,
        data: {
          ...dashResult.data!,
          currentWorkspaceId,
          workspace: workspace.data!,
          apps: workspace.data ? workspace.data.apps : [],
        },
        error:
          dashResult.error ||
          (workspace.error
            ? {
                message: workspace.error.message,
                workspaceId: currentWorkspaceId,
                retry: () => workspace.mutate(),
              }
            : null),
        mutate: dashResult.mutate,
        optimisticUpdate: dashResult.optimisticUpdate,
        optimisticUpdateWorkspace: workspace.optimisticUpdate,
        optimisticUpdateCurrentWorkspace,
        fromCache: dashResult.fromCache,
      };
    },
  );

export const MainDashLayout: React.FC<{
  children: ReactNode;
  className?: string;
}> = ({ children, className }) => {
  const token = useAuthToken();
  const router = useRouter();

  const tickets = useTicketSystem();
  const { darkMode } = useDarkMode();

  useEffect(() => {
    document.documentElement.classList.toggle('dark', darkMode);
    document.documentElement.dataset.dashboard = 'true';
    document.documentElement.style.colorScheme = darkMode ? 'dark' : 'light';
    return () => {
      delete document.documentElement.dataset.dashboard;
      document.documentElement.style.colorScheme = '';
    };
  }, [darkMode]);

  const handleVerified = () => {
    const returnTo = router.query['return-to'];
    if (
      returnTo &&
      typeof returnTo === 'string' &&
      // Prevent an open redirect
      returnTo.startsWith('/') &&
      !returnTo.startsWith('//')
    ) {
      router.replace(returnTo);
    }
  };

  if (!token) {
    return (
      <Auth
        key="anonymous"
        ticket={tickets.cliNormalTicket}
        onVerified={handleVerified}
      />
    );
  }

  return (
    <TokenContext.Provider key={token} value={token}>
      <PageHead>
        <meta name="theme-color" content={darkMode ? '#262626' : '#ffffff'} />
      </PageHead>
      <Toaster position="top-right" theme={darkMode ? 'dark' : 'light'} />
      <DashFetchProvider
        loading={<FullscreenLoading />}
        error={(error) => <WorkspaceLoadError error={error} />}
      >
        <div
          className={cn(
            'dashboard-shell fixed inset-0 flex flex-col bg-white text-gray-900 dark:bg-neutral-800 dark:text-white',
            darkMode ? 'dark' : '',
          )}
        >
          <a
            className="dashboard-skip-link"
            href="#dashboard-main"
            onClick={(event) => {
              event.preventDefault();
              const content =
                document.getElementById('dashboard-page-content') ??
                document.getElementById('dashboard-main');
              content?.focus();
            }}
          >
            Skip to content
          </a>
          <TopBar />
          <div
            id="dashboard-main"
            tabIndex={-1}
            className={cn(
              'dashboard-content flex min-h-0 w-full grow flex-col overflow-auto',
              className,
            )}
          >
            {children}
          </div>
        </div>
      </DashFetchProvider>
      <CLILoginDialog tickets={tickets} />
    </TokenContext.Provider>
  );
};

function WorkspaceLoadError({
  error,
}: {
  error: {
    message?: string;
    workspaceId?: string;
    retry?: () => Promise<unknown>;
  };
}) {
  const [retrying, setRetrying] = useState(false),
    [message, setMessage] = useState('');
  return (
    <section className="dashboard-shell shell-load-error">
      <p className="dashboard-resource-eyebrow">Workspace unavailable</p>
      <h1>
        {error.workspaceId
          ? 'Could not load this organization'
          : 'Could not load the dashboard'}
      </h1>
      <p role="alert">
        {message || error.message || 'The request failed. Try again.'}
      </p>
      {error.workspaceId && <code>{error.workspaceId}</code>}
      <div className="wb-actions">
        <button
          disabled={retrying}
          onClick={async () => {
            setRetrying(true);
            setMessage('');
            try {
              if (error.retry) await error.retry();
              else window.location.reload();
            } catch (e) {
              setMessage(
                e instanceof Error ? e.message : 'Refresh failed. Try again.',
              );
            } finally {
              setRetrying(false);
            }
          }}
        >
          {retrying ? 'Retrying…' : 'Retry'}
        </button>
        <a href="/dash?org=personal">Open personal workspace</a>
      </div>
    </section>
  );
}
