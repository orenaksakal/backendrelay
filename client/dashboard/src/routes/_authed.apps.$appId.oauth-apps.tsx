import { Outlet, createFileRoute } from '@tanstack/react-router';

/**
 * C.14 OAuth apps: the list (`/oauth-apps`) and one app
 * (`/oauth-apps/$oauthAppId`) share this prefix but render their own page.
 */
export const Route = createFileRoute('/_authed/apps/$appId/oauth-apps')({
  component: OauthAppsLayout,
});

function OauthAppsLayout() {
  return <Outlet />;
}
