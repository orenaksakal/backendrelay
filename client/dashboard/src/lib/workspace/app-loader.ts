import type { QueryClient } from '@tanstack/react-query';
import { notFound } from '@tanstack/react-router';
import { isApiError } from '@/lib/api/client';
import type { App, EffectiveRole } from '@/lib/api/endpoints/schemas';
import { PERSONAL_WORKSPACE_ID } from './workspaces';
import { appQueryOptions, bootstrapQueryOptions } from './queries';

/** What every `/apps/$appId` screen reads from its route context. */
export interface AppRouteContext {
  app: App;
  role: EffectiveRole;
  /** The org that owns the app, or `personal`. */
  workspaceId: string;
}

/** The effective role is always present at runtime; an app-member is the floor. */
export function roleForApp(app: App): EffectiveRole {
  return app.role ?? app.user_app_role ?? 'app-member';
}

export function workspaceIdForApp(app: App): string {
  return app.org?.id ?? app.org_id ?? PERSONAL_WORKSPACE_ID;
}

export function appRouteContext(app: App): AppRouteContext {
  return {
    app,
    role: roleForApp(app),
    workspaceId: workspaceIdForApp(app),
  };
}

/**
 * Resolves an app from the bootstrap payload or falls back to
 * `GET /dash/apps/:id` with a 15s timeout. A 404 becomes the route's not-found
 * state, which renders the app 404 ErrorState.
 */
export async function loadAppRouteContext(
  queryClient: QueryClient,
  appId: string,
): Promise<AppRouteContext> {
  const dash = await queryClient.query({
    ...bootstrapQueryOptions(),
    staleTime: 'static',
  });
  const bootstrapped = dash.apps.find((app) => app.id === appId);
  if (bootstrapped !== undefined) return appRouteContext(bootstrapped);

  try {
    const result = await queryClient.query(appQueryOptions(appId));
    return appRouteContext(result.app);
  } catch (error) {
    if (isApiError(error) && error.status === 404) throw notFound();
    throw error;
  }
}
