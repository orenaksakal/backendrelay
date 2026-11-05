import { Outlet, createFileRoute, redirect } from '@tanstack/react-router';
import { isLegacyDashIndex, legacyRedirect } from '@/lib/legacy-redirects';

/**
 * Layout for the old `/dash` URLs (B.6). The index redirects from the search
 * string; `dash.$` handles the subpaths, so this layout mostly renders them.
 */
export const Route = createFileRoute('/dash')({
  beforeLoad: ({ location }) => {
    if (!isLegacyDashIndex(location.pathname)) return;
    throw redirect({ href: legacyRedirect(location.href), replace: true });
  },
  component: LegacyDashLayout,
});

function LegacyDashLayout() {
  return <Outlet />;
}
