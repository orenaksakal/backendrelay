import { createFileRoute } from '@tanstack/react-router';
import { OrgSettingsPage } from '@/features/organization/settings-page';

/** C.17 org settings: rename and delete. */
export const Route = createFileRoute('/_authed/w/$workspaceId/settings')({
  component: OrgSettingsRoute,
});

function OrgSettingsRoute() {
  const { workspaceId } = Route.useParams();
  return <OrgSettingsPage workspaceId={workspaceId} />;
}
