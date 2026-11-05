import { createFileRoute } from '@tanstack/react-router';
import { AppSettingsPage } from '@/features/app-settings/app-settings-page';
import {
  appSettingsSearchSchema,
  type AppSettingsSearch,
} from '@/lib/apps/settings';
import { useAppRouteContext } from '@/lib/workspace/app-context';

/** C.16 app settings. `?tab=` selects general, token, team or danger zone. */
export const Route = createFileRoute('/_authed/apps/$appId/settings')({
  validateSearch: (search: Record<string, unknown>): AppSettingsSearch =>
    appSettingsSearchSchema.parse(search),
  component: AppSettingsRoute,
});

function AppSettingsRoute() {
  const appContext = useAppRouteContext();
  const { tab } = Route.useSearch();
  const navigate = Route.useNavigate();

  if (appContext === undefined) return null;

  return (
    <AppSettingsPage
      app={appContext.app}
      role={appContext.role}
      workspaceId={appContext.workspaceId}
      tab={tab}
      onTabChange={(next) => {
        void navigate({ search: (previous) => ({ ...previous, tab: next }) });
      }}
    />
  );
}
