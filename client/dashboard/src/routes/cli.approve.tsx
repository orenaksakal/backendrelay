import { createFileRoute, redirect } from '@tanstack/react-router';
import {
  CliApprovePage,
  type CliApproveSearch,
} from '@/features/auth/cli-approve-page';
import { getSession, startSession } from '@/lib/auth/session';

/**
 * C.1 CLI approval. The CLI sends the browser here with a ticket; the screen
 * needs a session, so a signed-out visitor signs in and comes back.
 *
 * Like `_authed`, the session lives in `localStorage`, so this branch renders
 * on the client only: a server render cannot see the session and would send a
 * signed-in operator back to `/login`.
 */
export const Route = createFileRoute('/cli/approve')({
  ssr: false,
  validateSearch: (search: Record<string, unknown>): CliApproveSearch => ({
    ticket: typeof search['ticket'] === 'string' ? search['ticket'] : undefined,
  }),
  beforeLoad: ({ location }) => {
    startSession();
    if (getSession() === null) {
      throw redirect({
        to: '/login',
        search: { redirect: location.href },
        replace: true,
      });
    }
  },
  head: () => ({ meta: [{ title: 'Approve CLI sign-in · BackendRelay' }] }),
  component: CliApproveRoute,
});

function CliApproveRoute() {
  const search = Route.useSearch();
  return <CliApprovePage search={search} />;
}
