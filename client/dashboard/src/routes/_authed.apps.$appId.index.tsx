import { createFileRoute, redirect } from '@tanstack/react-router';

/** `/apps/$appId` itself always opens the overview (B.6). */
export const Route = createFileRoute('/_authed/apps/$appId/')({
  beforeLoad: ({ params }) => {
    throw redirect({
      href: `/apps/${encodeURIComponent(params.appId)}/overview`,
      replace: true,
    });
  },
});
