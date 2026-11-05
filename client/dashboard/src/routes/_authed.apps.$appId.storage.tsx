import { createFileRoute } from '@tanstack/react-router';
import { useCallback } from 'react';
import { StoragePage } from '@/features/storage/storage-page';
import {
  storageSearchSchema,
  type StorageSearch,
} from '@/lib/storage/url-state';
import { useAppRouteContext } from '@/lib/workspace/app-context';

/** C.9 Storage: the open folder, search and sheet live in the URL. */
export const Route = createFileRoute('/_authed/apps/$appId/storage')({
  validateSearch: (search: Record<string, unknown>): StorageSearch =>
    storageSearchSchema.parse(search),
  component: StorageRoute,
});

function StorageRoute() {
  const appContext = useAppRouteContext();
  const search = Route.useSearch();
  const navigate = Route.useNavigate();

  const onSearchChange = useCallback(
    (next: StorageSearch) => {
      void navigate({ search: next });
    },
    [navigate],
  );

  if (appContext === undefined) return null;
  return (
    <StoragePage
      app={appContext.app}
      role={appContext.role}
      search={search}
      onSearchChange={onSearchChange}
    />
  );
}
