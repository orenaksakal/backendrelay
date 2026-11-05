import { createFileRoute } from '@tanstack/react-router';
import { SignInPage } from '@/features/auth/sign-in-page';
import type { SignInSearch } from '@/lib/auth/login';

/**
 * C.1 sign in (D4.1). The search contract predates this step because the
 * `_authed` layout already redirects here: `redirect` is a same-origin path and
 * `ticket` is a CLI login ticket.
 */
export const Route = createFileRoute('/login')({
  validateSearch: (search: Record<string, unknown>): SignInSearch => ({
    redirect: readSearchString(search['redirect']),
    ticket: readSearchString(search['ticket']),
  }),
  head: () => ({ meta: [{ title: 'Sign in · BackendRelay' }] }),
  component: LoginRoute,
});

function readSearchString(value: unknown): string | undefined {
  return typeof value === 'string' ? value : undefined;
}

function LoginRoute() {
  const search = Route.useSearch();
  return <SignInPage search={search} />;
}
