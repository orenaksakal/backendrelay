import { createFileRoute } from '@tanstack/react-router';
import { InvitesPage } from '@/features/account/invites-page';

/** C.18 pending invites. */
export const Route = createFileRoute('/_authed/account/invites')({
  component: InvitesPage,
});
