import { createFileRoute } from '@tanstack/react-router';
import { useCallback } from 'react';
import { PermissionsPage } from '@/features/permissions/permissions-page';
import {
  permissionsSearchSchema,
  type PermissionsSearch,
} from '@/lib/permissions/url-state';
import { useAppRouteContext } from '@/lib/workspace/app-context';

/** C.10 Permissions: the view is URL-backed; `?test=1` opens Test-as. */
export const Route = createFileRoute('/_authed/apps/$appId/permissions')({
  validateSearch: (search: Record<string, unknown>): PermissionsSearch =>
    permissionsSearchSchema.parse(search),
  component: PermissionsRoute,
});

function PermissionsRoute() {
  const appContext = useAppRouteContext();
  const search = Route.useSearch();
  const navigate = Route.useNavigate();

  const onSearchChange = useCallback(
    (next: PermissionsSearch) => {
      void navigate({ search: next });
    },
    [navigate],
  );

  if (appContext === undefined) return null;
  return (
    <PermissionsPage
      key={appContext.app.id}
      app={appContext.app}
      role={appContext.role}
      search={search}
      onSearchChange={onSearchChange}
    />
  );
}
