import { createFileRoute, redirect } from '@tanstack/react-router';
import { bootstrapQueryOptions } from '@/lib/workspace/queries';
import { landingPath, readLastWorkspace } from '@/lib/workspace/workspaces';

/**
 * `/` is a routing decision, never a screen: first run goes to `/welcome`, an
 * invite-only account to `/account/invites`, everyone else to their last
 * workspace (D3.3).
 */
export const Route = createFileRoute('/_authed/')({
  beforeLoad: async ({ context }) => {
    const dash = await context.queryClient.query({
      ...bootstrapQueryOptions(),
      staleTime: 'static',
    });
    throw redirect({
      href: landingPath(dash, readLastWorkspace()),
      replace: true,
    });
  },
});
