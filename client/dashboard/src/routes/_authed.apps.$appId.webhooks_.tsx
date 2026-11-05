import { createFileRoute } from '@tanstack/react-router';
import { WebhooksPage } from '@/features/webhooks/webhooks-page';
import { useAppRouteContext } from '@/lib/workspace/app-context';

/** C.13 webhook list. The create/edit sheet is local to the screen. */
export const Route = createFileRoute('/_authed/apps/$appId/webhooks_')({
  component: WebhooksRoute,
});

function WebhooksRoute() {
  const appContext = useAppRouteContext();

  if (appContext === undefined) return null;
  return (
    <WebhooksPage
      key={appContext.app.id}
      app={appContext.app}
      role={appContext.role}
    />
  );
}
