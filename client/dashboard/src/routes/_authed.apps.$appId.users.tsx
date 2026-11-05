import { createFileRoute } from '@tanstack/react-router';
import { useCallback } from 'react';
import { UsersPage } from '@/features/users/users-page';
import { usersSearchSchema, type UsersSearch } from '@/lib/users/url-state';
import { useAppRouteContext } from '@/lib/workspace/app-context';

/** C.8 Users: the email search, type filter and open sheet live in the URL. */
export const Route = createFileRoute('/_authed/apps/$appId/users')({
  validateSearch: (search: Record<string, unknown>): UsersSearch =>
    usersSearchSchema.parse(search),
  component: UsersRoute,
});

function UsersRoute() {
  const appContext = useAppRouteContext();
  const search = Route.useSearch();
  const navigate = Route.useNavigate();

  const onSearchChange = useCallback(
    (next: UsersSearch) => {
      void navigate({ search: next });
    },
    [navigate],
  );

  if (appContext === undefined) return null;
  return (
    <UsersPage
      app={appContext.app}
      role={appContext.role}
      search={search}
      onSearchChange={onSearchChange}
    />
  );
}
