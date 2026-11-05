import { useRouter, type NavigateOptions } from '@tanstack/react-router';
import { Mail, UserPlus, Users } from 'lucide-react';
import { useState } from 'react';
import { RelativeTime } from '@/components/data/relative-time';
import {
  SimpleTable,
  type SimpleTableColumn,
} from '@/components/data/simple-table';
import { ConfirmDialog } from '@/components/feedback/confirm-dialog';
import { EmptyState } from '@/components/feedback/empty-state';
import { ErrorState } from '@/components/feedback/error-state';
import { TableSkeleton } from '@/components/feedback/skeletons';
import { PageHeader } from '@/components/layout/page-header';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { accountTimestamp } from '@/lib/account/dates';
import { isApiError } from '@/lib/api/client';
import type { OrgDetails } from '@/lib/api/endpoints/schemas';
import { useSession } from '@/lib/auth/session';
import { orgErrorMessage } from '@/lib/organization/errors';
import {
  ORG_ROLE_LABELS,
  assignableRoles,
  canManageOrg,
  canRemoveMember,
  isLastOwner,
  isOrgRole,
} from '@/lib/organization/model';
import {
  useRemoveOrgMember,
  useRevokeOrgInvite,
  useUpdateOrgMember,
} from '@/lib/organization/queries';
import { useOrgDetails } from '@/lib/workspace/queries';
import { PERSONAL_WORKSPACE_ID } from '@/lib/workspace/workspaces';
import { InviteMemberDialog } from './invite-member-dialog';

const ROLE_ERROR = "Couldn't change the member's role. Try again.";
const REMOVE_ERROR = "Couldn't remove the member. Try again.";
const REVOKE_ERROR = "Couldn't revoke the invite. Try again.";

const LAST_OWNER_HINT =
  "The last owner can't be demoted or removed. Promote someone else first.";
const ROLE_LIMIT_HINT = 'Your role cannot change this member.';

export type OrgMembersTab = 'members' | 'invites';

export interface OrgMembersPageProps {
  workspaceId: string;
  tab: OrgMembersTab;
  onTabChange: (tab: OrgMembersTab) => void;
}

type OrgMember = OrgDetails['members'][number];
type OrgInvite = OrgDetails['invites'][number];

/**
 * C.17 members: the member table with role controls, the pending invite list
 * and the invite dialog. The server enforces the same rank checks; this screen
 * only explains them before the request.
 */
export function OrgMembersPage({
  workspaceId,
  tab,
  onTabChange,
}: OrgMembersPageProps) {
  const isPersonal = workspaceId === PERSONAL_WORKSPACE_ID;
  const org = useOrgDetails(isPersonal ? null : workspaceId);

  if (isPersonal) {
    return (
      <div className="flex max-w-4xl flex-col gap-4">
        <PageHeader title="Members" />
        <EmptyState
          icon={Users}
          title="Personal workspaces have no members"
          description="Create an organization to share apps and manage roles."
        />
      </div>
    );
  }

  if (org.isError) {
    return (
      <div className="flex max-w-4xl flex-col gap-4">
        <PageHeader title="Members" />
        <ErrorState
          thing="organization"
          status={isApiError(org.error) ? org.error.status : undefined}
          message={org.error instanceof Error ? org.error.message : undefined}
          requestId={isApiError(org.error) ? org.error.requestId : undefined}
          onRetry={() => {
            void org.refetch();
          }}
        />
      </div>
    );
  }

  if (org.data === undefined) {
    return (
      <div className="flex max-w-4xl flex-col gap-4">
        <PageHeader title="Members" />
        <TableSkeleton columns={4} />
      </div>
    );
  }

  return (
    <OrgMembersContent
      workspaceId={workspaceId}
      details={org.data}
      tab={tab}
      onTabChange={onTabChange}
    />
  );
}

interface OrgMembersContentProps extends OrgMembersPageProps {
  details: OrgDetails;
}

function OrgMembersContent({
  workspaceId,
  details,
  tab,
  onTabChange,
}: OrgMembersContentProps) {
  const router = useRouter();
  const session = useSession();
  const updateMember = useUpdateOrgMember(workspaceId);
  const removeMember = useRemoveOrgMember(workspaceId);
  const revokeInvite = useRevokeOrgInvite(workspaceId);
  const [inviteOpen, setInviteOpen] = useState(false);
  const [removeTarget, setRemoveTarget] = useState<OrgMember>();

  const myRole = details.org.role;
  const members = details.members;
  const invites = details.invites.filter(
    (invite) => invite.status === 'pending',
  );
  const canManage = canManageOrg(myRole);

  function isSelf(member: OrgMember): boolean {
    const email = session?.user.email;
    return (
      email !== undefined && email.toLowerCase() === member.email.toLowerCase()
    );
  }

  if (myRole === 'app-member') {
    return (
      <div className="flex max-w-4xl flex-col gap-4">
        <PageHeader title="Members" description={details.org.title} />
        <ErrorState
          thing="organization members"
          status={403}
          requiredRole="collaborator"
          message="Only members of this organization can see who else has access."
        />
      </div>
    );
  }

  const memberColumns: SimpleTableColumn<OrgMember>[] = [
    {
      id: 'email',
      header: 'Email',
      cell: (member) => (
        <span className="flex items-center gap-2">
          <span className="font-medium">{member.email}</span>
          {isSelf(member) ? <Badge tone="accent">You</Badge> : null}
        </span>
      ),
    },
    {
      id: 'role',
      header: 'Role',
      cell: (member) => (
        <MemberRoleCell
          myRole={myRole}
          member={member}
          members={members}
          pending={
            updateMember.isPending &&
            updateMember.variables.memberId === member.id
          }
          onChange={(role) => {
            updateMember.reset();
            updateMember.mutate({ memberId: member.id, role });
          }}
        />
      ),
    },
    {
      id: 'joined',
      header: 'Joined',
      cell: (member) =>
        member.created_at === undefined ? (
          <span className="text-muted-foreground">—</span>
        ) : (
          <RelativeTime
            value={accountTimestamp(member.created_at)}
            className="text-cell"
          />
        ),
    },
    {
      id: 'actions',
      header: <span className="sr-only">Actions</span>,
      align: 'right',
      cell: (member) => {
        const self = isSelf(member);
        const lastOwner = isLastOwner(members, member.id);
        const allowed =
          !lastOwner &&
          (self ||
            canRemoveMember({
              myRole,
              theirRole: member.role,
              lastOwner,
            }));
        const disabledReason = lastOwner ? LAST_OWNER_HINT : ROLE_LIMIT_HINT;

        return (
          <Button
            type="button"
            variant="ghost"
            size="sm"
            disabled={!allowed}
            title={allowed ? undefined : disabledReason}
            onClick={() => {
              removeMember.reset();
              setRemoveTarget(member);
            }}
          >
            {self ? 'Leave org' : 'Remove'}
          </Button>
        );
      },
    },
  ];

  const inviteColumns: SimpleTableColumn<OrgInvite>[] = [
    {
      id: 'email',
      header: 'Email',
      cell: (invite) => (
        <span className="flex items-center gap-2">
          <span className="font-medium">{invite.email}</span>
          {invite.expired ? <Badge tone="warning">Expired</Badge> : null}
        </span>
      ),
    },
    {
      id: 'role',
      header: 'Role',
      cell: (invite) => <span>{ORG_ROLE_LABELS[invite.role]}</span>,
    },
    {
      id: 'sent',
      header: 'Sent',
      cell: (invite) => (
        <RelativeTime
          value={accountTimestamp(invite.sent_at)}
          className="text-cell"
        />
      ),
    },
    {
      id: 'actions',
      header: <span className="sr-only">Actions</span>,
      align: 'right',
      cell: (invite) => {
        const revoking =
          revokeInvite.isPending && revokeInvite.variables === invite.id;
        return (
          <Button
            type="button"
            variant="ghost"
            size="sm"
            disabled={!canManage || revoking}
            aria-busy={revoking}
            title={canManage ? undefined : ROLE_LIMIT_HINT}
            onClick={() => {
              revokeInvite.mutate(invite.id);
            }}
          >
            Revoke
          </Button>
        );
      },
    },
  ];

  const targetSelf = removeTarget !== undefined && isSelf(removeTarget);

  return (
    <div className="flex max-w-4xl flex-col gap-4">
      <Tabs
        value={tab}
        className="flex flex-col gap-4"
        onValueChange={(value) => {
          if (value === 'members' || value === 'invites')
            onTabChange(value as OrgMembersTab);
        }}
      >
        <PageHeader
          title="Members"
          description="Manage who can see and change this organization's apps."
          actions={
            canManage ? (
              <Button
                type="button"
                onClick={() => {
                  setInviteOpen(true);
                }}
              >
                <UserPlus aria-hidden="true" />
                Invite member
              </Button>
            ) : undefined
          }
          tabs={
            <TabsList aria-label="Members and invites">
              <TabsTrigger value="members">
                Members
                <Badge tone="neutral">{members.length}</Badge>
              </TabsTrigger>
              <TabsTrigger value="invites">
                Invites
                {invites.length === 0 ? null : (
                  <Badge tone="neutral">{invites.length}</Badge>
                )}
              </TabsTrigger>
            </TabsList>
          }
        />

        <TabsContent value="members">
          <SimpleTable
            caption="Organization members"
            columns={memberColumns}
            rows={members}
            getRowId={(member) => member.id}
            empty="No members"
          />
        </TabsContent>

        <TabsContent value="invites">
          {invites.length === 0 ? (
            <EmptyState
              icon={Mail}
              title="No pending invites"
              description="Invite teammates by email to share this organization's apps."
            />
          ) : (
            <SimpleTable
              caption="Pending invites"
              columns={inviteColumns}
              rows={invites}
              getRowId={(invite) => invite.id}
              empty="No pending invites"
            />
          )}
        </TabsContent>
      </Tabs>

      {updateMember.isError ? (
        <p role="alert" className="text-destructive text-sm">
          {orgErrorMessage(updateMember.error, ROLE_ERROR)}
        </p>
      ) : null}
      {removeMember.isError ? (
        <p role="alert" className="text-destructive text-sm">
          {orgErrorMessage(removeMember.error, REMOVE_ERROR)}
        </p>
      ) : null}
      {revokeInvite.isError ? (
        <p role="alert" className="text-destructive text-sm">
          {orgErrorMessage(revokeInvite.error, REVOKE_ERROR)}
        </p>
      ) : null}

      {inviteOpen ? (
        <InviteMemberDialog
          orgId={workspaceId}
          myRole={myRole}
          members={members}
          invites={details.invites}
          onClose={() => {
            setInviteOpen(false);
          }}
        />
      ) : null}

      <ConfirmDialog
        open={removeTarget !== undefined}
        onOpenChange={(open) => {
          if (open) return;
          setRemoveTarget(undefined);
          removeMember.reset();
        }}
        title={targetSelf ? 'Leave organization?' : 'Remove member?'}
        description={
          targetSelf ? (
            <>
              Leave{' '}
              <strong className="font-semibold">{details.org.title}</strong>?
              You will lose access to its apps unless someone invites you back.
            </>
          ) : (
            <>
              Remove{' '}
              <strong className="font-semibold">{removeTarget?.email}</strong>{' '}
              from{' '}
              <strong className="font-semibold">{details.org.title}</strong>?
              They lose access immediately.
            </>
          )
        }
        confirmLabel={targetSelf ? 'Leave org' : 'Remove member'}
        confirming={removeMember.isPending}
        onConfirm={() => {
          if (removeTarget === undefined) return;
          const leaving = targetSelf;
          removeMember.mutate(removeTarget.id, {
            onSuccess: () => {
              setRemoveTarget(undefined);
              if (leaving) {
                void router.navigate({ href: '/' } as NavigateOptions);
              }
            },
          });
        }}
      />
    </div>
  );
}

interface MemberRoleCellProps {
  myRole: OrgDetails['org']['role'];
  member: OrgMember;
  members: OrgDetails['members'];
  pending: boolean;
  onChange: (role: OrgMember['role']) => void;
}

/**
 * A role select capped at the viewer's own role. The last owner is shown as
 * text with the reason, because the server refuses to demote them.
 */
function MemberRoleCell({
  myRole,
  member,
  members,
  pending,
  onChange,
}: MemberRoleCellProps) {
  if (isLastOwner(members, member.id)) {
    return (
      <span className="flex items-center gap-2">
        <span className="text-sm">{ORG_ROLE_LABELS[member.role]}</span>
        <Badge tone="neutral" title={LAST_OWNER_HINT}>
          Last owner
          <span className="sr-only"> — {LAST_OWNER_HINT}</span>
        </Badge>
      </span>
    );
  }

  const options = assignableRoles(myRole, member.role);
  if (options.length === 0) {
    return <span className="text-sm">{ORG_ROLE_LABELS[member.role]}</span>;
  }

  return (
    <Select
      value={member.role}
      disabled={pending}
      onValueChange={(value) => {
        if (!isOrgRole(value) || value === member.role) return;
        onChange(value);
      }}
    >
      <SelectTrigger aria-label={`Role for ${member.email}`} className="w-36">
        <SelectValue>{ORG_ROLE_LABELS[member.role]}</SelectValue>
      </SelectTrigger>
      <SelectContent>
        {options.map((option) => (
          <SelectItem key={option} value={option}>
            {ORG_ROLE_LABELS[option]}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}
