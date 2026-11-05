import { useCallback, useContext } from 'react';
import { asClientOnlyPage, useReadyRouter } from '@/components/clientOnlyPage';
import { useFetchedDash } from '@/components/dash/MainDashLayout';
import { CreationWorkspace } from '@/components/dash/creation/CreationWorkspace';
import {
  creationHref,
  targets,
  type Request,
} from '@/components/dash/creation/model';
import config from '@/lib/config';
import { workspaceHref } from '@/components/dash/shell/model';
import { TokenContext } from '@/lib/contexts';
import { jsonFetch } from '@/lib/fetch';
import { usePostHog } from 'posthog-js/react';

function NewApp() {
  const dash = useFetchedDash(),
    token = useContext(TokenContext),
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
    <CreationWorkspace
      kind="application"
      accountId={dash.data.user.id}
      availableTargets={targets(dash.data.orgs)}
      initialTarget={dash.data.currentWorkspaceId}
      request={request}
      onCreated={(result) => {
        posthog.capture('app_create', {
          app_id: result.id,
          is_first_app: dash.data.apps.length === 0,
        });
      }}
      onOpen={async (result) => {
        await dash.refetch();
        dash.setWorkspace(result.orgId ?? 'personal');
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
      onCancel={() =>
        router.push(workspaceHref('/dash', dash.data.currentWorkspaceId))
      }
    />
  );
}
export default asClientOnlyPage(NewApp);
export { createApp } from '@/components/dash/creation/api';
