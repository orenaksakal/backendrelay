import { createFileRoute, redirect } from '@tanstack/react-router';
import { AppShell } from '@/components/layout/app-shell';
import { getSession, startSession } from '@/lib/auth/session';

/**
 * Pathless layout for every signed-in screen. `beforeLoad` runs before the
 * child route's loaders, so protected data is never requested without a
 * session. The shell owns the top bar, sidebar and `main` landmark (D3.4).
 *
 * The session lives in `localStorage`, so a server render cannot know whether
 * the visitor is signed in. Rendering this branch only on the client keeps an
 * SSR pass (for example `vite preview` serving a deep link) from redirecting a
 * signed-in user to `/login` before the browser has restored the session.
 */
export const Route = createFileRoute('/_authed')({
  ssr: false,
  beforeLoad: ({ location }) => {
    // Restore first: hydration may reuse a server match that never ran this
    // hook, and the guard must decide with the stored session in hand.
    startSession();
    if (getSession() === null) {
      throw redirect({
        to: '/login',
        search: { redirect: location.href },
        replace: true,
      });
    }
  },
  component: AuthedLayout,
});

function AuthedLayout() {
  return <AppShell />;
}
