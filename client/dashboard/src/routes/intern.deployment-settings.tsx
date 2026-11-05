import { createFileRoute, redirect } from '@tanstack/react-router';
import { legacyRedirect } from '@/lib/legacy-redirects';

/** The old intern page became the operator settings screen (B.6). */
export const Route = createFileRoute('/intern/deployment-settings')({
  beforeLoad: ({ location }) => {
    throw redirect({ href: legacyRedirect(location.href), replace: true });
  },
});
