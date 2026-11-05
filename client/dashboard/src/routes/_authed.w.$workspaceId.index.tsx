import { createFileRoute } from '@tanstack/react-router';
import { AppsHomePage } from '@/features/apps/apps-home-page';
import {
  appsHomeSearchSchema,
  type AppsHomeSearch,
} from '@/lib/apps/apps-home';

/** C.4 apps home. `?sort=` and `?dir=` drive the table and the header buttons. */
export const Route = createFileRoute('/_authed/w/$workspaceId/')({
  validateSearch: (search: Record<string, unknown>): AppsHomeSearch =>
    appsHomeSearchSchema.parse(search),
  component: AppsHomeRoute,
});

function AppsHomeRoute() {
  const { workspaceId } = Route.useParams();
  const { sort, dir } = Route.useSearch();
  const navigate = Route.useNavigate();

  return (
    <AppsHomePage
      workspaceId={workspaceId}
      sort={sort}
      dir={dir}
      onSortChange={(nextSort, nextDir) => {
        void navigate({
          search: (previous) => ({
            ...previous,
            sort: nextSort,
            dir: nextDir,
          }),
        });
      }}
    />
  );
}
