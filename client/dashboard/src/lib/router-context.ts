import type { QueryClient } from '@tanstack/react-query';

/** Router context available to every route loader, including `/apps/$appId`. */
export interface RouterContext {
  queryClient: QueryClient;
}
