import { createFileRoute } from '@tanstack/react-router';
import { useCallback } from 'react';
import { WebhookDetailPage } from '@/features/webhooks/webhook-detail-page';
import {
  closedEventSearch,
  eventSearchFor,
  webhookDetailSearchSchema,
  type WebhookDetailSearch,
} from '@/lib/webhooks/url-state';
import { useAppRouteContext } from '@/lib/workspace/app-context';

/** C.13 webhook detail: the delivery sheet lives in `?event=`. */
export const Route = createFileRoute(
  '/_authed/apps/$appId/webhooks/$webhookId',
)({
  validateSearch: (search: Record<string, unknown>): WebhookDetailSearch =>
    webhookDetailSearchSchema.parse(search),
  component: WebhookDetailRoute,
});

function WebhookDetailRoute() {
  const appContext = useAppRouteContext();
  const { webhookId } = Route.useParams();
  const search = Route.useSearch();
  const navigate = Route.useNavigate();

  const onSelectedEventChange = useCallback(
    (isn: string | undefined) => {
      void navigate({
        search: (previous: WebhookDetailSearch) =>
          isn === undefined
            ? closedEventSearch(previous)
            : eventSearchFor(previous, isn),
      });
    },
    [navigate],
  );

  if (appContext === undefined) return null;
  return (
    <WebhookDetailPage
      key={`${appContext.app.id}:${webhookId}`}
      app={appContext.app}
      role={appContext.role}
      webhookId={webhookId}
      selectedEvent={search.event}
      onSelectedEventChange={onSelectedEventChange}
    />
  );
}
