import { createFileRoute } from '@tanstack/react-router';
import { AppOverviewPage } from '@/features/overview/overview-page';
import { useAppRouteContext } from '@/lib/workspace/app-context';

/** C.5 app overview; the app and effective role come from the app layout. */
export const Route = createFileRoute('/_authed/apps/$appId/overview')({
  component: AppOverviewRoute,
});

function AppOverviewRoute() {
  const appContext = useAppRouteContext();
  if (appContext === undefined) return null;
  return <AppOverviewPage app={appContext.app} role={appContext.role} />;
}
