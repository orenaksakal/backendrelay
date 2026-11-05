import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import {
  RouterProvider,
  createMemoryHistory,
  createRouter,
} from '@tanstack/react-router';
import { render as rtlRender } from '@testing-library/react';
import { routeTree } from '@/routeTree.gen';

export function createTestQueryClient(): QueryClient {
  return new QueryClient({
    defaultOptions: {
      queries: { retry: false, gcTime: 0, staleTime: 0 },
      mutations: { retry: false },
    },
  });
}

export interface RenderOptions {
  /** Initial history entry. Defaults to the index route. */
  route?: string;
  queryClient?: QueryClient;
}

/**
 * Renders the app router with the Query and Router providers used in
 * production. Tests get the same route tree as the browser, so loaders,
 * search params and `Link` behave normally.
 */
export function render({
  route = '/',
  queryClient = createTestQueryClient(),
}: RenderOptions = {}) {
  const router = createRouter({
    routeTree,
    history: createMemoryHistory({ initialEntries: [route] }),
    context: { queryClient },
    defaultPreload: false,
    scrollRestoration: false,
  });

  const result = rtlRender(
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
    { container: document as unknown as HTMLElement },
  );

  return { ...result, router, queryClient };
}
