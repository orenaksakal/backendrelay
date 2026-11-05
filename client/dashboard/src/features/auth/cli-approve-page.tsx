import { Loader2 } from 'lucide-react';
import { useRef, useState } from 'react';
import { Button, buttonVariants } from '@/components/ui/button';
import { apiFetch } from '@/lib/api/client';
import { sessionEndpoints } from '@/lib/api/endpoints';
import { authErrorMessage } from '@/lib/auth/oauth';
import { useSession } from '@/lib/auth/session';
import { AuthCard } from './auth-card';

/** `/cli/approve` search params (C.1). */
export interface CliApproveSearch {
  ticket?: string;
}

type Decision = 'approve' | 'deny';

const CLAIM_FALLBACK = "Couldn't approve this CLI sign-in. Try again.";
const VOID_FALLBACK = "Couldn't deny this CLI sign-in. Try again.";

/**
 * C.1 CLI approval. The CLI opened this page with a ticket; approving attaches
 * the signed-in user to it, denying burns it. Both results leave the terminal
 * to finish the login on its own.
 */
export function CliApprovePage({ search }: { search: CliApproveSearch }) {
  const session = useSession();
  const [pending, setPending] = useState<Decision | undefined>(undefined);
  const [result, setResult] = useState<Decision | undefined>(undefined);
  const [error, setError] = useState<string | undefined>(undefined);
  const busy = useRef(false);

  const ticket = search.ticket;

  async function decide(decision: Decision): Promise<void> {
    if (ticket === undefined || ticket === '' || busy.current) return;
    busy.current = true;
    setPending(decision);
    setError(undefined);
    try {
      if (decision === 'approve') {
        await apiFetch(sessionEndpoints.claimCliTicket, { body: { ticket } });
      } else {
        await apiFetch(sessionEndpoints.voidCliTicket, { body: { ticket } });
      }
      setResult(decision);
    } catch (caught) {
      setError(
        authErrorMessage(
          caught,
          decision === 'approve' ? CLAIM_FALLBACK : VOID_FALLBACK,
        ),
      );
    } finally {
      busy.current = false;
      setPending(undefined);
    }
  }

  if (result !== undefined) {
    return (
      <AuthCard
        title={
          result === 'approve' ? 'CLI sign-in approved' : 'CLI sign-in denied'
        }
      >
        <p className="text-sm">
          You can close this tab and return to your terminal.
        </p>
      </AuthCard>
    );
  }

  if (ticket === undefined || ticket === '') {
    return (
      <AuthCard title="This CLI sign-in link isn't valid">
        <div className="flex flex-col gap-4">
          <p role="alert" className="text-destructive text-sm">
            The link is missing its ticket. Start the CLI login again.
          </p>
          <a href="/" className={buttonVariants({ variant: 'secondary' })}>
            Back to BackendRelay
          </a>
        </div>
      </AuthCard>
    );
  }

  return (
    <AuthCard title="Sign in to the BackendRelay CLI?">
      <div className="flex flex-col gap-4">
        <p className="text-sm">
          A CLI on your computer is asking to act as{' '}
          <strong className="font-semibold">
            {session?.user.email ?? 'your account'}
          </strong>
          .
        </p>

        {error === undefined ? null : (
          <p role="alert" className="text-destructive text-sm">
            {error}
          </p>
        )}

        <div className="flex flex-wrap justify-end gap-2">
          <Button
            type="button"
            variant="secondary"
            disabled={pending !== undefined}
            onClick={() => {
              void decide('deny');
            }}
          >
            {pending === 'deny' ? (
              <Loader2 className="animate-spin" aria-hidden="true" />
            ) : null}
            Deny
          </Button>
          <Button
            type="button"
            disabled={pending !== undefined}
            aria-busy={pending === 'approve'}
            onClick={() => {
              void decide('approve');
            }}
          >
            {pending === 'approve' ? (
              <Loader2 className="animate-spin" aria-hidden="true" />
            ) : null}
            Approve
          </Button>
        </div>
      </div>
    </AuthCard>
  );
}
