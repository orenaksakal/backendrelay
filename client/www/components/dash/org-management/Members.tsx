import { useReadyRouter } from '@/components/clientOnlyPage';
import { useFetchedDash } from '../MainDashLayout';
import { Badge, Button, SubsectionHeading, useDialog } from '@/components/ui';
import { InviteToOrgDialog } from './InviteToOrgDialog';
import { isMinRole, Role } from '@/pages/dash';
import config from '@/lib/config';
import { jsonFetch } from '@/lib/fetch';
import { errorToast } from '@/lib/toast';
import { useAuthToken } from '@/lib/auth';
import { MemberMenu } from './MemberMenu';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/DropdownMenu';
import {
  ArrowUturnLeftIcon,
  EllipsisHorizontalIcon,
} from '@heroicons/react/24/solid';
import Link from '@/lib/navigation';

const READABLE_ROLES: Record<string, string> = {
  admin: 'Admin',
  collaborator: 'Collaborator',
  owner: 'Owner',
};

export const Members = () => {
  const dashResponse = useFetchedDash();
  const router = useReadyRouter();
  const org = dashResponse.data.workspace;

  const dialog = useDialog();
  const token = useAuthToken();

  const revoke = async (inviteId: string) => {
    if (org.type === 'personal') {
      throw new Error('Cannot revoke invite from personal workspace');
    }
    const responsePromise = jsonFetch(
      `${config.apiURI}/dash/orgs/${org.id}/invite/revoke`,
      {
        body: JSON.stringify({ 'invite-id': inviteId, 'org-id': org.id }),
        headers: {
          'Content-Type': 'application/json',
          Authorization: `Bearer ${token}`,
        },
        method: 'DELETE',
      },
    );

    await dashResponse.optimisticUpdateWorkspace(
      responsePromise,
      (workspace) => {
        if (workspace.type === 'personal') {
          throw new Error('Cannot revoke invite from personal workspace');
        }
        workspace.invites = workspace.invites.filter(
          (invite) => invite.id !== inviteId,
        );
      },
    );
  };

  if (
    dashResponse.data.currentWorkspaceId === 'personal' ||
    org.type === 'personal'
  ) {
    router.replace('/dash');
    return;
  }

  const invites = org.invites.filter(
    (invite) => invite.status !== 'accepted' && invite.status !== 'revoked',
  );
  const myRole = org.org.role as Role;
  const myEmail = dashResponse.data.user.email;

  return (
    <div className="">
      <div className="flex items-end justify-between py-2">
        <SubsectionHeading>Current Members</SubsectionHeading>
        {isMinRole('admin', myRole) && (
          <Button onClick={() => dialog.onOpen()} size="mini">
            Invite
          </Button>
        )}
      </div>
      <InviteToOrgDialog dialog={dialog} />
      <div className="divide-y rounded-xs border bg-white dark:divide-neutral-700 dark:border-neutral-700 dark:bg-neutral-800">
        {org.members.map((member) => (
          <div
            className="flex w-full items-center justify-between gap-2 rounded-xs p-2 transition-colors hover:bg-gray-50 dark:hover:bg-neutral-700/40"
            key={member.id}
          >
            <div className="flex items-center gap-3">
              {member.email}
              {member.email === myEmail && <Badge>Me</Badge>}
            </div>
            <div className="flex items-center gap-3">
              <div className="text-sm">{READABLE_ROLES[member.role]}</div>
              <MemberMenu member={member} />
            </div>
          </div>
        ))}
      </div>

      <div className="mt-6">
        <SubsectionHeading>Pending Invites</SubsectionHeading>
        {invites.length === 0 ? (
          <div className="w-full py-8 text-center text-sm opacity-50">
            No pending invites
          </div>
        ) : (
          <div className="divide-y border bg-white dark:border-neutral-700 dark:bg-neutral-800">
            {invites.map((invite) => (
              <div
                className="flex w-full items-center justify-between gap-2 p-2 transition-colors hover:bg-gray-50 dark:hover:bg-neutral-700/40"
                key={invite.id}
              >
                <div>{invite.email}</div>
                <DropdownMenu>
                  <DropdownMenuTrigger>
                    <EllipsisHorizontalIcon opacity={'50%'} width={20} />
                  </DropdownMenuTrigger>
                  <DropdownMenuContent>
                    <DropdownMenuItem>
                      <button
                        type="button"
                        onClick={() => {
                          void revoke(invite.id).catch(() =>
                            errorToast('Failed to revoke invite.'),
                          );
                        }}
                        className="flex gap-2"
                      >
                        <ArrowUturnLeftIcon width={14} />
                        Revoke Invite
                      </button>
                    </DropdownMenuItem>
                  </DropdownMenuContent>
                </DropdownMenu>
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
};
