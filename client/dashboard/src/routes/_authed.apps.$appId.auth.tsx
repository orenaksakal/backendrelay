import { createFileRoute } from '@tanstack/react-router';
import { useCallback } from 'react';
import { AppAuthPage } from '@/features/app-auth/app-auth-page';
import {
  appAuthSearchSchema,
  type AppAuthSearch,
} from '@/lib/app-auth/url-state';
import { useAppRouteContext } from '@/lib/workspace/app-context';

/** C.11 Authentication: the tab and sheet live in the URL. */
export const Route = createFileRoute('/_authed/apps/$appId/auth')({
  validateSearch: (search: Record<string, unknown>): AppAuthSearch =>
    appAuthSearchSchema.parse(search),
  component: AppAuthRoute,
});

function AppAuthRoute() {
  const appContext = useAppRouteContext();
  const search = Route.useSearch();
  const navigate = Route.useNavigate();

  const onSearchChange = useCallback(
    (next: AppAuthSearch) => {
      void navigate({ search: next });
    },
    [navigate],
  );

  if (appContext === undefined) return null;
  return (
    <AppAuthPage
      key={appContext.app.id}
      app={appContext.app}
      role={appContext.role}
      search={search}
      onSearchChange={onSearchChange}
    />
  );
}
