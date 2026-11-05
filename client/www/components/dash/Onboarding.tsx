import { useCallback, useContext } from 'react';
import { usePostHog } from 'posthog-js/react';
import { useReadyRouter } from '../clientOnlyPage';
import { useFetchedDash } from './MainDashLayout';
import { OnboardingWorkspace } from './creation/OnboardingWorkspace';
import { creationHref, type Request } from './creation/model';
import config, { isSelfHosted } from '@/lib/config';
import { TokenContext } from '@/lib/contexts';
import { jsonFetch } from '@/lib/fetch';
export { createApp } from './creation/api';
export function Onboarding() {
  const token = useContext(TokenContext),
    dash = useFetchedDash(),
    router = useReadyRouter(),
    posthog = usePostHog();
  const request: Request = useCallback(
    (path, init) =>
      jsonFetch(`${config.apiURI}${path}`, {
        ...init,
        headers: {
          ...init?.headers,
          authorization: `Bearer ${token}`,
          'content-type': 'application/json',
        },
      }),
    [token],
  );
  return (
    <OnboardingWorkspace
      accountId={dash.data.user.id}
      selfHosted={isSelfHosted}
      request={request}
      onCreated={(result, meta) => {
        posthog.capture('onboarding_complete', {
          heard_from: meta?.heard,
          experience_level: meta?.experience,
        });
        posthog.capture('app_create', {
          app_id: result.id,
          is_first_app: true,
        });
      }}
      onOpen={async (result) => {
        await dash.refetch();
        dash.setWorkspace('personal');
        router.replace(
          creationHref(result, {
            ticket:
              typeof router.query.ticket === 'string'
                ? router.query.ticket
                : undefined,
            _cli_oauth_ticket:
              typeof router.query._cli_oauth_ticket === 'string'
                ? router.query._cli_oauth_ticket
                : undefined,
          }),
        );
      }}
      onCancel={() => router.push('/dash/user-settings')}
    />
  );
}
