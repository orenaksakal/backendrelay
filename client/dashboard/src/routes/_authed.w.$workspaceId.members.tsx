import { createFileRoute } from '@tanstack/react-router';
import {
  OrgMembersPage,
  type OrgMembersTab,
} from '@/features/organization/members-page';

export interface OrgMembersSearch {
  tab: OrgMembersTab;
}

/** C.17 org members. `?tab=` selects the member or invite list. */
export const Route = createFileRoute('/_authed/w/$workspaceId/members')({
  validateSearch: (search: Record<string, unknown>): OrgMembersSearch => ({
    tab: search['tab'] === 'invites' ? 'invites' : 'members',
  }),
  component: OrgMembersRoute,
});

function OrgMembersRoute() {
  const { workspaceId } = Route.useParams();
  const { tab } = Route.useSearch();
  const navigate = Route.useNavigate();

  return (
    <OrgMembersPage
      workspaceId={workspaceId}
      tab={tab}
      onTabChange={(next) => {
        void navigate({ search: (previous) => ({ ...previous, tab: next }) });
      }}
    />
  );
}
