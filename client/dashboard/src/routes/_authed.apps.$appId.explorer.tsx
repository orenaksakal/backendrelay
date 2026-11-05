import { createFileRoute } from '@tanstack/react-router';
import { useCallback } from 'react';
import { ExplorerPage } from '@/features/explorer/explorer-page';
import {
  explorerSearchSchema,
  type ExplorerSearch,
} from '@/lib/explorer/url-state';
import { useAppRouteContext } from '@/lib/workspace/app-context';

/** C.6 Explorer: namespace, filters, sort and page size live in the URL. */
export const Route = createFileRoute('/_authed/apps/$appId/explorer')({
  validateSearch: (search: Record<string, unknown>): ExplorerSearch =>
    explorerSearchSchema.parse(search),
  component: ExplorerRoute,
});

function ExplorerRoute() {
  const appContext = useAppRouteContext();
  const search = Route.useSearch();
  const navigate = Route.useNavigate();

  const onSearchChange = useCallback(
    (next: ExplorerSearch, options?: { replace?: boolean }) => {
      void navigate({ search: next, replace: options?.replace });
    },
    [navigate],
  );

  if (appContext === undefined) return null;
  return (
    <ExplorerPage
      app={appContext.app}
      role={appContext.role}
      search={search}
      onSearchChange={onSearchChange}
    />
  );
}
