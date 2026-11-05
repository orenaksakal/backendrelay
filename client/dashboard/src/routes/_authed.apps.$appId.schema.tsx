import { createFileRoute } from '@tanstack/react-router';
import { useCallback } from 'react';
import { SchemaPage } from '@/features/schema/schema-page';
import { schemaSearchSchema, type SchemaSearch } from '@/lib/schema/url-state';
import { useAppRouteContext } from '@/lib/workspace/app-context';

/** C.7 Schema: the selected namespace and view live in the URL. */
export const Route = createFileRoute('/_authed/apps/$appId/schema')({
  validateSearch: (search: Record<string, unknown>): SchemaSearch =>
    schemaSearchSchema.parse(search),
  component: SchemaRoute,
});

function SchemaRoute() {
  const appContext = useAppRouteContext();
  const search = Route.useSearch();
  const navigate = Route.useNavigate();

  const onSearchChange = useCallback(
    (next: SchemaSearch) => {
      void navigate({ search: next });
    },
    [navigate],
  );

  if (appContext === undefined) return null;
  return (
    <SchemaPage
      app={appContext.app}
      role={appContext.role}
      search={search}
      onSearchChange={onSearchChange}
    />
  );
}
