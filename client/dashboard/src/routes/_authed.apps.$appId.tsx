import {
  Outlet,
  createFileRoute,
  type ErrorComponentProps,
} from '@tanstack/react-router';
import { useEffect } from 'react';
import { ErrorState } from '@/components/feedback/error-state';
import { rememberRecentApp } from '@/lib/app-usage';
import { isApiError } from '@/lib/api/client';
import { loadAppRouteContext } from '@/lib/workspace/app-loader';

/**
 * App-scoped layout: resolves `{app, role, workspaceId}` once for every
 * `/apps/$appId/*` screen. A missing app shows the 404 ErrorState (D3.3).
 */
export const Route = createFileRoute('/_authed/apps/$appId')({
  loader: ({ context, params }) =>
    loadAppRouteContext(context.queryClient, params.appId),
  notFoundComponent: AppNotFound,
  errorComponent: AppRouteError,
  component: AppLayout,
});

function AppLayout() {
  const { app } = Route.useLoaderData();

  // A real visit, not a prefetch, feeds the palette's recent apps (B.7).
  useEffect(() => {
    rememberRecentApp(app.id);
  }, [app.id]);

  return <Outlet />;
}

function AppNotFound() {
  return <ErrorState thing="app" status={404} kind="app" />;
}

function AppRouteError({ error, reset }: ErrorComponentProps) {
  const apiError = isApiError(error) ? error : undefined;
  return (
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
      onRetry={reset}
    />
  );
}
