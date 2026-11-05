import { Mail } from 'lucide-react';
import { useRouter, type NavigateOptions } from '@tanstack/react-router';
import {
  SimpleTable,
  type SimpleTableColumn,
} from '@/components/data/simple-table';
import { EmptyState } from '@/components/feedback/empty-state';
import { ErrorState } from '@/components/feedback/error-state';
import { PageHeader } from '@/components/layout/page-header';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { accountErrorMessage } from '@/lib/account/errors';
import { inviteDestination, inviteTypeLabel } from '@/lib/account/invites';
import { useAcceptInvite, useDeclineInvite } from '@/lib/account/queries';
import { isApiError } from '@/lib/api/client';
import type { PendingInvite } from '@/lib/api/endpoints/schemas';
import { useBootstrap } from '@/lib/workspace/queries';

const ACCEPT_ERROR = "Couldn't accept the invite. Try again.";
const DECLINE_ERROR = "Couldn't decline the invite. Try again.";

/**
 * C.18 Invites: pending app and org invitations from the bootstrap payload.
 * Accepting opens the invited workspace; declining removes the row.
 */
export function InvitesPage() {
  const router = useRouter();
  const bootstrap = useBootstrap();
  const accept = useAcceptInvite();
  const decline = useDeclineInvite();

  const rows = bootstrap.data?.invites ?? [];

  const columns: SimpleTableColumn<PendingInvite>[] = [
    {
      id: 'workspace',
      header: 'Workspace',
      cell: (invite) => (
        <span className="flex items-center gap-2">
          <span className="font-medium">{invite.title}</span>
          <Badge tone="neutral">{inviteTypeLabel(invite.type)}</Badge>
        </span>
      ),
    },
    {
      id: 'role',
      header: 'Role',
      cell: (invite) => (
        <span className="capitalize">{invite.invitee_role}</span>
      ),
    },
    {
      id: 'inviter',
      header: 'Invited by',
      cell: (invite) => (
        <span className="text-cell">{invite.inviter_email}</span>
      ),
    },
    {
      id: 'actions',
      header: <span className="sr-only">Actions</span>,
      align: 'right',
      cell: (invite) => {
        const accepting = accept.isPending && accept.variables === invite.id;
        const declining = decline.isPending && decline.variables === invite.id;
        const busy = accepting || declining;

        return (
          <span className="flex justify-end gap-1">
            <Button
              type="button"
              size="sm"
              disabled={busy}
              aria-busy={accepting}
              onClick={() => {
                accept.mutate(invite.id, {
                  onSuccess: () => {
                    void router.navigate({
                      href: inviteDestination(invite),
                    } as NavigateOptions);
                  },
                });
              }}
            >
              Accept
            </Button>
            <Button
              type="button"
              variant="secondary"
              size="sm"
              disabled={busy}
              aria-busy={declining}
              onClick={() => {
                decline.mutate(invite.id);
              }}
            >
              Decline
            </Button>
          </span>
        );
      },
    },
  ];

  return (
    <div className="flex max-w-4xl flex-col gap-4">
      <PageHeader
        title="Invites"
        description="Apps and organizations waiting for your reply."
      />

      {bootstrap.isError ? (
        <ErrorState
          thing="invites"
          status={
            isApiError(bootstrap.error) ? bootstrap.error.status : undefined
          }
          message={
            bootstrap.error instanceof Error
              ? bootstrap.error.message
              : undefined
          }
          requestId={
            isApiError(bootstrap.error) ? bootstrap.error.requestId : undefined
          }
          onRetry={() => {
            void bootstrap.refetch();
          }}
        />
      ) : rows.length === 0 && !bootstrap.isPending ? (
        <EmptyState
          icon={Mail}
          title="No pending invites"
          description="When someone invites you to an app or organization, it appears here."
        />
      ) : (
        <SimpleTable
          caption="Pending invites"
          columns={columns}
          rows={rows}
          getRowId={(invite) => invite.id}
          loading={bootstrap.isPending}
          empty="No pending invites"
        />
      )}

      {accept.isError ? (
        <p role="alert" className="text-destructive text-sm">
          {accountErrorMessage(accept.error, ACCEPT_ERROR)}
        </p>
      ) : null}
      {decline.isError ? (
        <p role="alert" className="text-destructive text-sm">
          {accountErrorMessage(decline.error, DECLINE_ERROR)}
        </p>
      ) : null}
    </div>
  );
}
