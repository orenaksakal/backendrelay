import { createFileRoute } from '@tanstack/react-router';
import { OauthAppDetailPage } from '@/features/oauth-apps/oauth-app-detail-page';
import { useAppRouteContext } from '@/lib/workspace/app-context';

/** C.14 OAuth app detail: app info, clients and client secrets. */
export const Route = createFileRoute(
  '/_authed/apps/$appId/oauth-apps/$oauthAppId',
)({
  component: OauthAppDetailRoute,
});

function OauthAppDetailRoute() {
  const appContext = useAppRouteContext();
  const { oauthAppId } = Route.useParams();
  if (appContext === undefined) return null;
  return (
    <OauthAppDetailPage
      app={appContext.app}
      role={appContext.role}
      oauthAppId={oauthAppId}
    />
  );
}
