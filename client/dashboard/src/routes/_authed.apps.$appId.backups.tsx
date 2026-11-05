import { createFileRoute } from '@tanstack/react-router';
import { BackupsPage } from '@/features/backups/backups-page';
import { backupSearchSchema, type BackupSearch } from '@/lib/backups/model';
import { useAppRouteContext } from '@/lib/workspace/app-context';

/** C.15 backups: `?tab=` selects snapshots, schedule or jobs. */
export const Route = createFileRoute('/_authed/apps/$appId/backups')({
  validateSearch: (search: Record<string, unknown>): BackupSearch =>
    backupSearchSchema.parse(search),
  component: BackupsRoute,
});

function BackupsRoute() {
  const appContext = useAppRouteContext();
  const { tab } = Route.useSearch();
  const navigate = Route.useNavigate();

  if (appContext === undefined) return null;

  return (
    <BackupsPage
      app={appContext.app}
      role={appContext.role}
      tab={tab}
      onTabChange={(next) => {
        void navigate({ search: (previous) => ({ ...previous, tab: next }) });
      }}
    />
  );
}
