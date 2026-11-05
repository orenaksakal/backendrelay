import { createRouter } from '@tanstack/react-router';
import { startSession } from '@/lib/auth/session';
import { getQueryClient } from '@/lib/query-client';
import { loadRuntimeConfig } from '@/lib/runtime-config';
import { enableMockApi } from '@/test/msw/enable';
import { routeTree } from './routeTree.gen';

// Start the worker as soon as the client entry loads; loaders that need mocks
// can await `enableMockApi()` before fetching.
if (!import.meta.env.SSR) {
  void enableMockApi().catch((error: unknown) => {
    console.error('Failed to start the mock API', error);
  });
}

export function getRouter() {
  return createRouter({
    routeTree,
    context: { queryClient: getQueryClient() },
    scrollRestoration: true,
    // The SPA shell is prerendered in Node with a placeholder config, and the
    // router reuses that root match on hydration, so the root `beforeLoad` does
    // not run for the first page. Load the container's `/config.json` (and
    // start the session) before the router renders anything.
    hydrate: async () => {
      await enableMockApi();
      await loadRuntimeConfig();
      startSession();
    },
  });
}

declare module '@tanstack/react-router' {
  interface Register {
    router: ReturnType<typeof getRouter>;
  }
}
