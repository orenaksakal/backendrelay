import { useRouter, type NavigateOptions } from '@tanstack/react-router';
import { Mail, UserPlus } from 'lucide-react';
import { useEffect, useId, useRef, useState } from 'react';
import { RelativeTime } from '@/components/data/relative-time';
import {
  SimpleTable,
  type SimpleTableColumn,
} from '@/components/data/simple-table';
import { ConfirmDialog } from '@/components/feedback/confirm-dialog';
import { ErrorState } from '@/components/feedback/error-state';
import { TableSkeleton } from '@/components/feedback/skeletons';
import { SectionCard } from '@/components/layout/section-card';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Field, FieldError, FieldLabel } from '@/components/ui/field';
import { Input } from '@/components/ui/input';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { accountTimestamp } from '@/lib/account/dates';
import type {
  App,
  EffectiveRole,
  OrgDetails,
} from '@/lib/api/endpoints/schemas';
import {
  APP_TEAM_ROLE_LABELS,
  appInviteProblem,
  appMemberRoleLabel,
  assignableAppRoles,
  canManageAppTeam,
  isAppTeamRole,
  type AppTeamRole,
} from '@/lib/apps/settings';
import {
  useInviteAppMember,
  useRemoveAppMember,
  useRevokeAppInvite,
  useUpdateAppMember,
} from '@/lib/apps/settings-queries';
import { useSession } from '@/lib/auth/session';
import { useOrgDetails } from '@/lib/workspace/queries';
import { toastManager } from '@/lib/toast';

const ROLE_ERROR = "Couldn't change the member's role. Try again.";
const REMOVE_ERROR = "Couldn't remove the member. Try again.";
const REVOKE_ERROR = "Couldn't revoke the invite. Try again.";
const INVITE_ERROR = "Couldn't send the invite. Try again.";

type AppMember = NonNullable<App['members']>[number];
type AppInvite = NonNullable<App['invites']>[number];
type OrgMember = OrgDetails['members'][number];

export interface AppTeamSectionProps {
  app: App;
  role: EffectiveRole;
  workspaceId: string;
}

/**
 * C.16 team tab. App members keep their own roles on top of the org; org
 * members are read-only here because the org page owns them.
 */
export function AppTeamSection({
  app,
  role,
  workspaceId,
}: AppTeamSectionProps) {
  if (!canManageAppTeam(role)) {
    return (
      <ErrorState
        thing="the app team"
        status={403}
        requiredRole="admin"
        message="Only app administrators and owners can manage members."
      />
    );
  }

  return <AppTeamContent app={app} role={role} workspaceId={workspaceId} />;
}

type AppTeamContentProps = AppTeamSectionProps;

function AppTeamContent({ app, role, workspaceId }: AppTeamContentProps) {
  const router = useRouter();
  const session = useSession();
  const org = useOrgDetails(app.org?.id ?? null);
  const updateMember = useUpdateAppMember(app.id);
  const removeMember = useRemoveAppMember(app.id);
  const revokeInvite = useRevokeAppInvite(app.id);
  const [inviteOpen, setInviteOpen] = useState(false);
  const [removeTarget, setRemoveTarget] = useState<AppMember>();

  const members = app.members ?? [];
  const invites = (app.invites ?? []).filter(
    (invite) => invite.status === 'pending',
  );
  const roleOptions = assignableAppRoles(role);

  function isSelf(member: AppMember): boolean {
    const email = session?.user.email;
    return (
      email !== undefined && email.toLowerCase() === member.email.toLowerCase()
    );
  }

  const memberColumns: SimpleTableColumn<AppMember>[] = [
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
      cell: (member) =>
        member.role === 'owner' ? (
          <span className="text-sm">Owner</span>
        ) : (
          <Select
            value={member.role}
            disabled={
              updateMember.isPending &&
              updateMember.variables.memberId === member.id
            }
            onValueChange={(value) => {
              if (!isAppTeamRole(value) || value === member.role) return;
              updateMember.reset();
              updateMember.mutate({ memberId: member.id, role: value });
            }}
          >
            <SelectTrigger
              aria-label={`Role for ${member.email}`}
              className="w-36"
            >
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {roleOptions.map((option) => (
                <SelectItem key={option} value={option}>
                  {APP_TEAM_ROLE_LABELS[option]}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        ),
    },
    {
      id: 'actions',
      header: <span className="sr-only">Actions</span>,
      align: 'right',
      cell: (member) => (
        <Button
          type="button"
          variant="ghost"
          size="sm"
          onClick={() => {
            removeMember.reset();
            setRemoveTarget(member);
          }}
        >
          {isSelf(member) ? 'Leave app' : 'Remove'}
        </Button>
      ),
    },
  ];

  const inviteColumns: SimpleTableColumn<AppInvite>[] = [
    {
      id: 'email',
      header: 'Email',
      cell: (invite) => <span className="font-medium">{invite.email}</span>,
    },
    {
      id: 'role',
      header: 'Role',
      cell: (invite) => <span>{appMemberRoleLabel(invite.role)}</span>,
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
            disabled={revoking}
            aria-busy={revoking}
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

  const orgMemberColumns: SimpleTableColumn<OrgMember>[] = [
    {
      id: 'email',
      header: 'Email',
      cell: (member) => member.email,
    },
    {
      id: 'role',
      header: 'Role',
      cell: (member) => appMemberRoleLabel(member.role),
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
  ];

  return (
    <div className="flex flex-col gap-4">
      {app.org ? (
        <SectionCard
          title="Organization members"
          description={
            <>
              Manage org members in{' '}
              <a
                href={`/w/${encodeURIComponent(app.org.id)}/members`}
                className="text-accent hover:underline"
              >
                {app.org.title}
              </a>{' '}
              → Members.
            </>
          }
        >
          {org.isPending ? (
            <TableSkeleton rows={3} columns={3} />
          ) : org.isError ? (
            <p role="alert" className="text-destructive text-sm">
              Couldn&apos;t load the organization members.
            </p>
          ) : (
            <SimpleTable
              caption="Organization members"
              columns={orgMemberColumns}
              rows={org.data.members}
              getRowId={(member) => member.id}
              empty="No organization members"
            />
          )}
        </SectionCard>
      ) : null}

      <SectionCard
        title="App members"
        description="People with access to this app only."
        actions={
          <Button
            type="button"
            onClick={() => {
              setInviteOpen(true);
            }}
          >
            <UserPlus aria-hidden="true" />
            Invite member
          </Button>
        }
      >
        <SimpleTable
          caption="App members"
          columns={memberColumns}
          rows={members}
          getRowId={(member) => member.id}
          empty="No app members"
        />
      </SectionCard>

      <SectionCard
        title="Pending invites"
        description="Invitations expire after three days."
      >
        {invites.length === 0 ? (
          <p className="text-muted-foreground flex items-center gap-2 text-sm">
            <Mail className="size-4" aria-hidden="true" />
            No pending invites
          </p>
        ) : (
          <SimpleTable
            caption="Pending invites"
            columns={inviteColumns}
            rows={invites}
            getRowId={(invite) => invite.id}
            empty="No pending invites"
          />
        )}
      </SectionCard>

      {updateMember.isError ? (
        <p role="alert" className="text-destructive text-sm">
          {ROLE_ERROR}
        </p>
      ) : null}
      {removeMember.isError ? (
        <p role="alert" className="text-destructive text-sm">
          {REMOVE_ERROR}
        </p>
      ) : null}
      {revokeInvite.isError ? (
        <p role="alert" className="text-destructive text-sm">
          {REVOKE_ERROR}
        </p>
      ) : null}

      {inviteOpen ? (
        <InviteAppMemberDialog
          app={app}
          myRole={role}
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
        title={targetSelf ? 'Leave app?' : 'Remove member?'}
        description={
          targetSelf ? (
            <>
              Leave <strong className="font-semibold">{app.title}</strong>? You
              will lose access unless someone invites you back.
            </>
          ) : (
            <>
              Remove{' '}
              <strong className="font-semibold">{removeTarget?.email}</strong>{' '}
              from <strong className="font-semibold">{app.title}</strong>? They
              lose access immediately.
            </>
          )
        }
        confirmLabel={targetSelf ? 'Leave app' : 'Remove member'}
        confirming={removeMember.isPending}
        onConfirm={() => {
          if (removeTarget === undefined) return;
          const leaving = targetSelf;
          removeMember.mutate(removeTarget.id, {
            onSuccess: () => {
              setRemoveTarget(undefined);
              if (leaving) {
                void router.navigate({
                  href: `/w/${encodeURIComponent(workspaceId)}`,
                } as NavigateOptions);
              }
            },
          });
        }}
      />
    </div>
  );
}

interface InviteAppMemberDialogProps {
  app: App;
  myRole: EffectiveRole;
  onClose: () => void;
}

function InviteAppMemberDialog({
  app,
  myRole,
  onClose,
}: InviteAppMemberDialogProps) {
  const invite = useInviteAppMember(app.id);
  const options = assignableAppRoles(myRole);
  const [email, setEmail] = useState('');
  const [role, setRole] = useState<AppTeamRole>(
    options.at(-1) ?? 'collaborator',
  );
  const [error, setError] = useState<string>();
  const emailRef = useRef<HTMLInputElement>(null);
  const emailId = useId();
  const errorId = `${emailId}-error`;
  const roleId = useId();

  useEffect(() => {
    emailRef.current?.focus();
  }, []);

  function submit(): void {
    if (invite.isPending) return;
    const problem = appInviteProblem({
      email,
      role,
      myRole,
      members: app.members ?? [],
      invites: app.invites ?? [],
    });
    if (problem !== undefined) {
      setError(problem);
      emailRef.current?.focus();
      return;
    }
    setError(undefined);
    invite.mutate(
      { email: email.trim().toLowerCase(), role },
      {
        onSuccess: () => {
          toastManager.add({ type: 'success', title: 'Invite sent' });
          onClose();
        },
      },
    );
  }

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
    >
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Invite member</DialogTitle>
          <DialogDescription>
            They receive an email and join this app with the role you choose.
            Invites expire after three days.
          </DialogDescription>
        </DialogHeader>
        <form
          className="mt-4 flex flex-col gap-4"
          onSubmit={(event) => {
            event.preventDefault();
            submit();
          }}
        >
          <Field invalid={error !== undefined}>
            <FieldLabel htmlFor={emailId}>Email</FieldLabel>
            <Input
              id={emailId}
              ref={emailRef}
              type="email"
              autoComplete="off"
              value={email}
              disabled={invite.isPending}
              aria-invalid={error !== undefined}
              aria-describedby={error === undefined ? undefined : errorId}
              onChange={(event) => {
                setEmail(event.target.value);
                setError(undefined);
                invite.reset();
              }}
            />
            {error === undefined ? null : (
              <FieldError id={errorId} role="alert">
                {error}
              </FieldError>
            )}
          </Field>

          <Field>
            <FieldLabel htmlFor={roleId}>Role</FieldLabel>
            <Select
              value={role}
              disabled={invite.isPending}
              onValueChange={(value) => {
                if (!isAppTeamRole(value)) return;
                setRole(value);
                setError(undefined);
              }}
            >
              <SelectTrigger id={roleId}>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {options.map((option) => (
                  <SelectItem key={option} value={option}>
                    {APP_TEAM_ROLE_LABELS[option]}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </Field>

          {invite.isError ? (
            <p role="alert" className="text-destructive text-sm">
              {INVITE_ERROR}
            </p>
          ) : null}

          <DialogFooter className="mt-0">
            <Button
              type="button"
              variant="secondary"
              disabled={invite.isPending}
              onClick={onClose}
            >
              Cancel
            </Button>
            <Button
              type="submit"
              disabled={invite.isPending || email.trim() === ''}
              aria-busy={invite.isPending}
            >
              <UserPlus aria-hidden="true" />
              Send invite
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
