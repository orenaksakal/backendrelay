import { queryOptions, useQuery } from '@tanstack/react-query';
import { apiFetch } from '@/lib/api/client';
import { appEndpoints } from '@/lib/api/endpoints';

/** C.7 Deleted tab: soft-deleted attributes and the restore grace period. */

export const schemaQueryKeys = {
  softDeleted: (appId: string) =>
    ['dash', 'apps', appId, 'soft-deleted-attrs'] as const,
};

export function softDeletedAttributesQueryOptions(appId: string) {
  return queryOptions({
    queryKey: schemaQueryKeys.softDeleted(appId),
    queryFn: ({ signal }) =>
      apiFetch(appEndpoints.softDeletedAttributes, {
        params: { appId },
        signal,
      }),
  });
}

export function useSoftDeletedAttributes(appId: string, enabled = true) {
  return useQuery({
    ...softDeletedAttributesQueryOptions(appId),
    enabled,
  });
}
