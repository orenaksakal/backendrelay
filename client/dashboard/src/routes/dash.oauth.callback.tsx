import { createFileRoute } from '@tanstack/react-router';
import {
  OAuthCallbackPage,
  type OAuthCallbackSearch,
} from '@/features/auth/oauth-callback-page';

/**
 * C.1 Google callback (contract path). The server redirects here with `code`,
 * `error` and an optional CLI `ticket`.
 */
export const Route = createFileRoute('/dash/oauth/callback')({
  validateSearch: (search: Record<string, unknown>): OAuthCallbackSearch => ({
    code: readSearchString(search['code']),
    error: readSearchString(search['error']),
    ticket: readSearchString(search['ticket']),
  }),
  head: () => ({ meta: [{ title: 'Signing in · BackendRelay' }] }),
  component: OAuthCallbackRoute,
});

function readSearchString(value: unknown): string | undefined {
  return typeof value === 'string' ? value : undefined;
}

function OAuthCallbackRoute() {
  const search = Route.useSearch();
  return <OAuthCallbackPage search={search} />;
}
