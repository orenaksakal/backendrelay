import { queryOptions, useQuery } from '@tanstack/react-query';
import { apiFetch } from '@/lib/api/client';
import { sessionEndpoints } from '@/lib/api/endpoints';

/**
 * The unauthenticated public config probe (D0.1). It only tells the sign-in
 * screen which options exist, so a failure hides the Google button instead of
 * blocking email sign-in.
 */
export function publicConfigQueryOptions() {
  return queryOptions({
    queryKey: ['dash', 'config'] as const,
    queryFn: ({ signal }) =>
      apiFetch(sessionEndpoints.publicConfig, { signal }),
  });
}

export function usePublicConfig() {
  return useQuery(publicConfigQueryOptions());
}
