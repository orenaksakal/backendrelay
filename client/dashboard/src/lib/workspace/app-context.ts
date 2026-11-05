import { useRouterState } from '@tanstack/react-router';
import type { AppRouteContext } from './app-loader';

/**
 * The `{app, role, workspaceId}` value from the `/apps/$appId` layout loader.
 * The shell reads it to label the breadcrumb and title without another request.
 */
export function useAppRouteContext(): AppRouteContext | undefined {
  const matches = useRouterState({ select: (state) => state.matches });
  const match = matches.find(
    (candidate) => candidate.routeId === '/_authed/apps/$appId',
  );
  return match?.loaderData;
}
