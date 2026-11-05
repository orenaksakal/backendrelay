import { createFileRoute, redirect } from '@tanstack/react-router';

/** `/account` is a routing decision: the profile screen is the first page. */
export const Route = createFileRoute('/_authed/account/')({
  beforeLoad: () => {
    throw redirect({ to: '/account/profile', replace: true });
  },
});
