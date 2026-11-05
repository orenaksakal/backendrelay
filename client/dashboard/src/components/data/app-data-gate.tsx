import type { ReactNode } from 'react';
import { ErrorState } from '@/components/feedback/error-state';
import type { App, EffectiveRole } from '@/lib/api/endpoints/schemas';
import { appDataAccess, useAppClient } from '@/lib/instant';
import { getRuntimeConfig } from '@/lib/runtime-config';

export interface AppDataGateProps {
  app: App;
  role: EffectiveRole;
  /** Screen name in the 403 copy, for example "explorer" (C.6). */
  thing: string;
  /** Overrides the 403 sentence when the screen name reads as a plural. */
  forbiddenMessage?: string;
  children: ReactNode;
}

/**
 * D6.1 data gate: the Explorer, Schema, Users and Storage screens need the
 * app's admin token and at least the admin role (C.6–C.9). Screens render their
 * own content only once the shared Instant client exists; below the admin role
 * they get the 403 ErrorState, and an admin whose payload has no token gets the
 * reload message instead of a half-connected screen.
 */
export function AppDataGate({
  app,
  role,
  thing,
  forbiddenMessage,
  children,
}: AppDataGateProps) {
  const access = appDataAccess(
    {
      appId: app.id,
      role,
      adminToken: app.admin_token,
      apiBaseUrl: getRuntimeConfig()?.apiBaseUrl,
    },
    thing,
  );
  const client = useAppClient(access.status === 'ready' ? access.config : null);

  if (access.status === 'forbidden') {
    return (
      <ErrorState
        thing={thing}
        status={403}
        requiredRole="admin"
        message={forbiddenMessage ?? access.message}
      />
    );
  }
  if (access.status === 'missing-token' || access.status === 'missing-config') {
    return <ErrorState thing={thing} message={access.message} />;
  }
  if (client === null) return null;
  return <>{children}</>;
}
