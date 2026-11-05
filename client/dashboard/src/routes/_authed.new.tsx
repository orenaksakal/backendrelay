import { createFileRoute } from '@tanstack/react-router';
import { NewAppPage } from '@/features/creation/new-app-page';

export interface NewAppSearch {
  workspace?: string;
}

/** C.3 create app (D4.4). `?workspace=` preselects Personal or an org. */
export const Route = createFileRoute('/_authed/new')({
  validateSearch: (search: Record<string, unknown>): NewAppSearch => ({
    workspace:
      typeof search['workspace'] === 'string' ? search['workspace'] : undefined,
  }),
  component: NewAppRoute,
});

function NewAppRoute() {
  const { workspace } = Route.useSearch();
  return <NewAppPage workspaceId={workspace} />;
}
