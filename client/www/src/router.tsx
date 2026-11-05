import { createRouter } from '@tanstack/react-router';
import { parseDashboardSearch } from './searchParams';
import { routeTree } from './routeTree.gen';

export function getRouter() {
  return createRouter({
    routeTree,
    scrollRestoration: true,
    parseSearch: parseDashboardSearch,
  });
}

declare module '@tanstack/react-router' {
  interface Register {
    router: ReturnType<typeof getRouter>;
  }
}
