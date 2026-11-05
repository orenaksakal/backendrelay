import { useQuery } from '@tanstack/react-query';
import { X } from 'lucide-react';
import { useEffect, useState, type ReactNode } from 'react';
import { ErrorState } from '@/components/feedback/error-state';
import { TableSkeleton } from '@/components/feedback/skeletons';
import { IconButton } from '@/components/layout/icon-button';
import { Button } from '@/components/ui/button';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { SignInPage } from '@/features/auth/sign-in-page';
import { isApiError } from '@/lib/api/client';
import { signOut, useSession } from '@/lib/auth/session';
import { roleForApp } from '@/lib/workspace/app-loader';
import { appQueryOptions } from '@/lib/workspace/queries';
import { DevtoolExplorer } from './devtool-explorer';
import { DevtoolHelp } from './devtool-help';
import { DevtoolPlayground } from './devtool-playground';
import { closeDevtool } from './model';

const SECTIONS = ['explorer', 'playground', 'help'] as const;
type DevtoolSection = (typeof SECTIONS)[number];

function isSection(value: unknown): value is DevtoolSection {
  return (
    typeof value === 'string' &&
    (SECTIONS as ReadonlyArray<string>).includes(value)
  );
}

export interface DevtoolPageProps {
  appId: string | undefined;
}

/**
 * C.20 `/_devtool?appId=`: the SDK opens this page in a 400×600 iframe. It
 * keeps the shell out, signs in inline, and gives Explorer, Playground and help
 * at that size. The close button speaks the SDK's postMessage protocol.
 */
export function DevtoolPage({ appId }: DevtoolPageProps) {
  const session = useSession();
  const [section, setSection] = useState<DevtoolSection>('explorer');
  const hasAppId = appId !== undefined && appId !== '';
  const appQuery = useQuery({
    ...appQueryOptions(appId ?? ''),
    enabled: session !== null && hasAppId,
  });
  const app = appQuery.data?.app;

  // The SDK's toggle shortcut also works while the iframe has focus.
  useEffect(() => {
    function onKeyDown(event: KeyboardEvent): void {
      if (!event.shiftKey || !event.ctrlKey) return;
      if (event.code !== 'Digit0') return;
      event.preventDefault();
      closeDevtool();
    }

    window.addEventListener('keydown', onKeyDown);
    return () => {
      window.removeEventListener('keydown', onKeyDown);
    };
  }, []);

  return (
    <main
      id="main"
      tabIndex={-1}
      className="bg-background flex h-dvh flex-col overflow-hidden"
    >
      <header className="border-border bg-sidebar flex h-10 shrink-0 items-center gap-2 border-b px-3">
        <span className="min-w-0 flex-1 truncate text-sm font-semibold">
          {app?.title ?? 'BackendRelay devtool'}
        </span>
        <IconButton
          aria-label="Close devtool"
          icon={<X aria-hidden="true" />}
          size="sm"
          onClick={() => {
            closeDevtool();
          }}
        />
      </header>

      {session === null ? (
        <div className="min-h-0 flex-1 overflow-auto p-3">
          <SignInPage search={{}} embedded />
        </div>
      ) : !hasAppId ? (
        <DevtoolNotice
          title="No app ID provided"
          message={
            <>
              We didn't receive an app ID. Check that you passed an{' '}
              <code className="font-mono">appId</code> to{' '}
              <code className="font-mono">init</code>.
            </>
          }
        />
      ) : appQuery.isError ? (
        <AppProblem
          appId={appId}
          email={session.user.email}
          error={appQuery.error}
          onRetry={() => {
            void appQuery.refetch();
          }}
        />
      ) : app === undefined ? (
        <div className="flex flex-col gap-2 p-3">
          <h1 className="sr-only">BackendRelay devtool</h1>
          <p role="status" className="sr-only">
            Loading app…
          </p>
          <TableSkeleton rows={5} columns={2} />
        </div>
      ) : (
        <>
          <h1 className="sr-only">{`Devtool for ${app.title}`}</h1>
          <Tabs
            value={section}
            onValueChange={(value) => {
              if (isSection(value)) setSection(value);
            }}
            className="flex min-h-0 flex-1 flex-col"
          >
            <TabsList aria-label="Devtool sections" className="mx-3 shrink-0">
              <TabsTrigger value="explorer">Explorer</TabsTrigger>
              <TabsTrigger value="playground">Playground</TabsTrigger>
              <TabsTrigger value="help">Help</TabsTrigger>
            </TabsList>
            <TabsContent
              value="explorer"
              className="min-h-0 flex-1 overflow-auto p-3 pt-0"
            >
              <DevtoolExplorer app={app} role={roleForApp(app)} />
            </TabsContent>
            <TabsContent
              value="playground"
              className="min-h-0 flex-1 overflow-auto p-3 pt-0"
            >
              <DevtoolPlayground app={app} role={roleForApp(app)} />
            </TabsContent>
            <TabsContent
              value="help"
              className="min-h-0 flex-1 overflow-auto p-3 pt-0"
            >
              <DevtoolHelp app={app} />
            </TabsContent>
          </Tabs>
        </>
      )}
    </main>
  );
}

function DevtoolNotice({
  title,
  message,
}: {
  title: string;
  message: ReactNode;
}) {
  return (
    <div className="flex min-h-0 flex-1 items-center justify-center overflow-auto p-4">
      <div className="max-w-sm space-y-3">
        <h1 className="text-page">{title}</h1>
        <p className="text-muted-foreground text-sm">{message}</p>
        <Button
          type="button"
          variant="secondary"
          onClick={() => {
            signOut();
          }}
        >
          Sign out
        </Button>
      </div>
    </div>
  );
}

function AppProblem({
  appId,
  email,
  error,
  onRetry,
}: {
  appId: string;
  email: string;
  error: unknown;
  onRetry: () => void;
}) {
  const apiError = isApiError(error) ? error : undefined;

  if (apiError?.status === 404) {
    return (
      <div className="flex min-h-0 flex-1 items-center justify-center overflow-auto p-4">
        <div className="max-w-sm space-y-3">
          <h1 className="text-page">We couldn't find your app</h1>
          <p className="text-muted-foreground text-sm">
            {`You're signed in as ${email}. We tried to open `}
            <code className="font-mono">{appId}</code>.
          </p>
          <p className="text-muted-foreground text-sm">
            Are you sure you have access? Contact the app owner, or sign out and
            use a different account.
          </p>
          <Button
            type="button"
            variant="secondary"
            onClick={() => {
              signOut();
            }}
          >
            Sign out
          </Button>
        </div>
      </div>
    );
  }

  return (
    <div className="min-h-0 flex-1 overflow-auto p-3">
      <ErrorState
        thing="app"
        status={apiError?.status}
        message={
          apiError?.message ??
          (error instanceof Error ? error.message : undefined)
        }
        type={apiError?.type}
        hint={apiError?.hint}
        requestId={apiError?.requestId}
        onRetry={onRetry}
      />
    </div>
  );
}
