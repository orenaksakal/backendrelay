import { createFileRoute } from '@tanstack/react-router';
import { OauthAppsPage } from '@/features/oauth-apps/oauth-apps-page';
import { useAppRouteContext } from '@/lib/workspace/app-context';

/** C.14 OAuth app list. */
export const Route = createFileRoute('/_authed/apps/$appId/oauth-apps/')({
  component: OauthAppsRoute,
});

function OauthAppsRoute() {
  const appContext = useAppRouteContext();
  if (appContext === undefined) return null;
  return <OauthAppsPage app={appContext.app} role={appContext.role} />;
}
