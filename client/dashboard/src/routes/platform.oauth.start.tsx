import { createFileRoute } from '@tanstack/react-router';
import {
  OAuthConsentPage,
  type OAuthConsentSearch,
} from '@/features/auth/oauth-consent-page';

/**
 * C.2 OAuth consent (contract path). It stays outside `_authed` so a signed-out
 * visitor gets the sign-in card in place instead of the app shell.
 */
export const Route = createFileRoute('/platform/oauth/start')({
  validateSearch: (search: Record<string, unknown>): OAuthConsentSearch => ({
    'redirect-id':
      typeof search['redirect-id'] === 'string'
        ? search['redirect-id']
        : undefined,
  }),
  head: () => ({ meta: [{ title: 'Authorize app · BackendRelay' }] }),
  component: OAuthConsentRoute,
});

function OAuthConsentRoute() {
  const search = Route.useSearch();
  return <OAuthConsentPage search={search} />;
}
