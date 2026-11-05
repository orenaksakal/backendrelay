import {
  queryOptions,
  useMutation,
  useQuery,
  useQueryClient,
} from '@tanstack/react-query';
import { apiFetch } from '@/lib/api/client';
import {
  operatorEndpoints,
  type OperatorFlags,
  type RestoreJob,
} from '@/lib/api/endpoints';
import type { OperatorSavePlan } from './model';

/**
 * C.19 data. Flags are one record on the config app, so a save writes only the
 * changed values back into that cache entry and the form's baseline follows.
 */

export const operatorQueryKeys = {
  flags: ['dash', 'operator', 'flags'] as const,
  restoreJobs: ['dash', 'restore-jobs'] as const,
};

export interface RestoreJobsResponse {
  'restore-jobs': RestoreJob[];
}

export function operatorFlagsQueryOptions() {
  return queryOptions({
    queryKey: operatorQueryKeys.flags,
    queryFn: ({ signal }) => apiFetch(operatorEndpoints.flags, { signal }),
  });
}

export function useOperatorFlags() {
  return useQuery(operatorFlagsQueryOptions());
}

/**
 * Writes the flag changes one request at a time, in a stable order, so a
 * partial failure leaves an easily re-read cache rather than racing updates.
 */
export function useSaveOperatorFlags() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async (plan: OperatorSavePlan) => {
      for (const write of plan.writes) {
        await apiFetch(operatorEndpoints.setFlag, {
          params: { name: write.name },
          body: { value: write.value },
        });
      }
      for (const name of plan.resets) {
        await apiFetch(operatorEndpoints.deleteFlag, { params: { name } });
      }
      return plan;
    },
    onSuccess: (plan) => {
      queryClient.setQueryData<OperatorFlags>(
        operatorQueryKeys.flags,
        (current) => {
          if (current === undefined) return current;
          const flags = { ...current.flags };
          for (const write of plan.writes) {
            flags[write.name] = write.value;
          }
          for (const name of plan.resets) {
            Reflect.deleteProperty(flags, name);
          }
          return { ...current, flags };
        },
      );
    },
    // A failure part way through leaves the cache behind the server.
    onError: () => {
      void queryClient.invalidateQueries({ queryKey: operatorQueryKeys.flags });
    },
  });
}

export function restoreJobsQueryOptions() {
  return queryOptions({
    queryKey: operatorQueryKeys.restoreJobs,
    queryFn: ({ signal }) =>
      apiFetch(operatorEndpoints.restoreJobs, { signal }),
  });
}

export function useRestoreJobs() {
  return useQuery(restoreJobsQueryOptions());
}

export function useCancelRestoreJob() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (jobId: string) =>
      apiFetch(operatorEndpoints.cancelRestoreJob, { params: { jobId } }),
    onSuccess: (result) => {
      const cancelled = result['restore-job'];
      if (cancelled === null) return;
      queryClient.setQueryData<RestoreJobsResponse>(
        operatorQueryKeys.restoreJobs,
        (current) =>
          current === undefined
            ? current
            : {
                'restore-jobs': current['restore-jobs'].map((job) =>
                  job.id === cancelled.id ? cancelled : job,
                ),
              },
      );
    },
  });
}
