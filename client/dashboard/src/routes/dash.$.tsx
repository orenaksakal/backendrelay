import { createFileRoute, redirect } from '@tanstack/react-router';
import { legacyRedirect } from '@/lib/legacy-redirects';

/** `/dash/new`, `/dash/onboarding`, `/dash/org` and friends (B.6). */
export const Route = createFileRoute('/dash/$')({
  beforeLoad: ({ location }) => {
    throw redirect({ href: legacyRedirect(location.href), replace: true });
  },
});
