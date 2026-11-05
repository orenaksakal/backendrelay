import { Loader2, UserPlus } from 'lucide-react';
import { useEffect, useId, useRef, useState } from 'react';
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
import type { EffectiveRole, OrgDetails } from '@/lib/api/endpoints/schemas';
import { orgErrorMessage } from '@/lib/organization/errors';
import {
  ORG_ROLE_LABELS,
  inviteProblem,
  inviteRoles,
  isOrgRole,
  type OrgRole,
} from '@/lib/organization/model';
import { useInviteOrgMember } from '@/lib/organization/queries';
import { toastManager } from '@/lib/toast';

const INVITE_ERROR = "Couldn't send the invite. Try again.";

export interface InviteMemberDialogProps {
  orgId: string;
  myRole: EffectiveRole;
  members: OrgDetails['members'];
  invites: OrgDetails['invites'];
  onClose: () => void;
}

/**
 * C.17 invite dialog. The role list stops at the inviter's own role and the
 * obvious duplicates are refused before the request, which would otherwise
 * silently upsert the pending invite.
 */
export function InviteMemberDialog({
  orgId,
  myRole,
  members,
  invites,
  onClose,
}: InviteMemberDialogProps) {
  const invite = useInviteOrgMember(orgId);
  const options = inviteRoles(myRole);
  const [email, setEmail] = useState('');
  const [role, setRole] = useState<OrgRole>(options.at(-1) ?? 'collaborator');
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
    const problem = inviteProblem({ email, role, myRole, members, invites });
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
            They receive an email and join with the role you choose. Invites
            expire after three days.
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
              <FieldError id={errorId} match role="alert">
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
                if (!isOrgRole(value)) return;
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
                    {ORG_ROLE_LABELS[option]}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </Field>

          {invite.isError ? (
            <p role="alert" className="text-destructive text-sm">
              {orgErrorMessage(invite.error, INVITE_ERROR)}
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
              {invite.isPending ? (
                <Loader2 className="animate-spin" aria-hidden="true" />
              ) : (
                <UserPlus aria-hidden="true" />
              )}
              Send invite
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
