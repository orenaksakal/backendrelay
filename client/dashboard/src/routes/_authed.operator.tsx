import { Outlet, createFileRoute } from '@tanstack/react-router';
import { ErrorState } from '@/components/feedback/error-state';
import { bootstrapQueryOptions } from '@/lib/workspace/queries';

/**
 * C.19 layout: `/operator/*` is superuser only. The bootstrap query is already
 * loaded by the shell, so the check adds no request; a non-superuser gets the
 * 403 state instead of the page.
 */
export const Route = createFileRoute('/_authed/operator')({
  loader: ({ context }) =>
    context.queryClient.query({
      ...bootstrapQueryOptions(),
      staleTime: 'static',
    }),
  component: OperatorLayout,
});

function OperatorLayout() {
  const bootstrap = Route.useLoaderData();

  if (!bootstrap.superuser) {
    return (
      <ErrorState
        thing="operator settings"
        status={403}
        message="Only the deployment operator can use this area."
      />
    );
  }

  return <Outlet />;
}
