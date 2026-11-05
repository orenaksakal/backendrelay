import { createFileRoute } from '@tanstack/react-router';
import { useCallback } from 'react';
import { PlaygroundPage } from '@/features/playground/playground-page';
import {
  playgroundSearchSchema,
  type PlaygroundSearch,
} from '@/lib/playground/url-state';
import { useAppRouteContext } from '@/lib/workspace/app-context';

/** C.12 Playground: the mode and "Run as" actor live in the URL. */
export const Route = createFileRoute('/_authed/apps/$appId/playground')({
  validateSearch: (search: Record<string, unknown>): PlaygroundSearch =>
    playgroundSearchSchema.parse(search),
  component: PlaygroundRoute,
});

function PlaygroundRoute() {
  const appContext = useAppRouteContext();
  const search = Route.useSearch();
  const navigate = Route.useNavigate();

  const onSearchChange = useCallback(
    (next: PlaygroundSearch) => {
      void navigate({ search: next });
    },
    [navigate],
  );

  if (appContext === undefined) return null;
  return (
    <PlaygroundPage
      key={appContext.app.id}
      app={appContext.app}
      role={appContext.role}
      search={search}
      onSearchChange={onSearchChange}
    />
  );
}
