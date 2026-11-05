import {
  type QueryClient,
  queryOptions,
  useMutation,
  useQuery,
  useQueryClient,
} from '@tanstack/react-query';
import { useEffect, useRef } from 'react';
import { apiFetch } from '@/lib/api/client';
import {
  backupEndpoints,
  type Backup,
  type BackupJob,
  type BackupSchedule,
} from '@/lib/api/endpoints';

/**
 * C.15 data. Active backup jobs and restore jobs poll while any job is
 * running; the snapshot list follows those jobs so a finished backup appears
 * without a manual refresh.
 */

export const backupQueryKeys = {
  list: (appId: string) => ['dash', 'apps', appId, 'backups'] as const,
  schedule: (appId: string) =>
    ['dash', 'apps', appId, 'backup-schedule'] as const,
  jobs: (appId: string) => ['dash', 'apps', appId, 'backup-jobs'] as const,
  restoreJobs: (appId: string) =>
    ['dash', 'apps', appId, 'restore-jobs'] as const,
};

export function backupsQueryOptions(appId: string, poll = false) {
  return queryOptions({
    queryKey: backupQueryKeys.list(appId),
    queryFn: ({ signal }) =>
      apiFetch(backupEndpoints.list, { params: { appId }, signal }),
    refetchInterval: poll ? 3_000 : false,
  });
}

export function useBackups(appId: string, poll = false) {
  return useQuery(backupsQueryOptions(appId, poll));
}

/**
 * Refreshes the snapshot list when the last active backup job finishes. The
 * list only polls while a job is active, so without this the snapshots taken
 * during those jobs would only show up after a manual reload. The created job
 * is written into the jobs cache before the first poll, so a job that finishes
 * between the create response and that poll still counts as active here.
 */
export function useRefreshBackupsOnJobCompletion(
  appId: string,
  hasActiveJob: boolean,
): void {
  const queryClient = useQueryClient();
  const hadActiveJob = useRef(hasActiveJob);

  useEffect(() => {
    if (hadActiveJob.current && !hasActiveJob) {
      void queryClient.invalidateQueries({
        queryKey: backupQueryKeys.list(appId),
      });
    }
    hadActiveJob.current = hasActiveJob;
  }, [appId, hasActiveJob, queryClient]);
}

export function backupJobsQueryOptions(appId: string) {
  return queryOptions({
    queryKey: backupQueryKeys.jobs(appId),
    queryFn: ({ signal }) =>
      apiFetch(backupEndpoints.jobs, { params: { appId }, signal }),
    refetchInterval: (query) =>
      (query.state.data?.jobs.length ?? 0) > 0 ? 2_000 : false,
  });
}

export function useBackupJobs(appId: string) {
  return useQuery(backupJobsQueryOptions(appId));
}

export function restoreJobsQueryOptions(appId: string) {
  return queryOptions({
    queryKey: backupQueryKeys.restoreJobs(appId),
    queryFn: ({ signal }) =>
      apiFetch(backupEndpoints.restoreJobs, { params: { appId }, signal }),
    refetchInterval: (query) =>
      (query.state.data?.['restore-jobs'] ?? []).some(
        (job) =>
          job.job_status === 'waiting' || job.job_status === 'processing',
      )
        ? 2_000
        : false,
  });
}

export function useAppRestoreJobs(appId: string) {
  return useQuery(restoreJobsQueryOptions(appId));
}

export function backupScheduleQueryOptions(appId: string) {
  return queryOptions({
    queryKey: backupQueryKeys.schedule(appId),
    queryFn: ({ signal }) =>
      apiFetch(backupEndpoints.getSchedule, { params: { appId }, signal }),
  });
}

export function useBackupSchedule(appId: string) {
  return useQuery(backupScheduleQueryOptions(appId));
}

function updateJobs(
  queryClient: QueryClient,
  appId: string,
  update: (jobs: BackupJob[]) => BackupJob[],
): void {
  queryClient.setQueryData<{ jobs: BackupJob[] }>(
    backupQueryKeys.jobs(appId),
    (current) =>
      current === undefined ? current : { jobs: update(current.jobs) },
  );
}

export function useCreateBackup(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (description: string) =>
      apiFetch(backupEndpoints.create, {
        params: { appId },
        body:
          description.trim() === '' ? {} : { description: description.trim() },
      }),
    onSuccess: ({ job }) => {
      updateJobs(queryClient, appId, (jobs) => [
        job,
        ...jobs.filter((current) => current.id !== job.id),
      ]);
      void queryClient.invalidateQueries({
        queryKey: backupQueryKeys.jobs(appId),
      });
    },
  });
}

export function useDeleteBackup(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (backupId: string) =>
      apiFetch(backupEndpoints.remove, { params: { appId, backupId } }),
    onSuccess: (_result, backupId) => {
      queryClient.setQueryData<{ backups: Backup[] }>(
        backupQueryKeys.list(appId),
        (current) =>
          current === undefined
            ? current
            : {
                backups: current.backups.filter(
                  (backup) => backup.id !== backupId,
                ),
              },
      );
    },
  });
}

export function useCancelBackupJob(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (jobId: string) =>
      apiFetch(backupEndpoints.cancelJob, { params: { appId, jobId } }),
    onSuccess: (_result, jobId) => {
      updateJobs(queryClient, appId, (jobs) =>
        jobs.filter((job) => job.id !== jobId),
      );
    },
  });
}

export function useSetBackupSchedule(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (value: { cron: string; enabled: boolean }) =>
      apiFetch(backupEndpoints.setSchedule, { params: { appId }, body: value }),
    onSuccess: (result) => {
      queryClient.setQueryData<{ schedule: BackupSchedule | null }>(
        backupQueryKeys.schedule(appId),
        result,
      );
    },
  });
}

export function useRestoreBackup(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ backupId, title }: { backupId: string; title: string }) =>
      apiFetch(backupEndpoints.restore, {
        params: { appId, backupId },
        body: title.trim() === '' ? {} : { title: title.trim() },
      }),
    onSuccess: () => {
      void queryClient.invalidateQueries({
        queryKey: backupQueryKeys.restoreJobs(appId),
      });
      void queryClient.invalidateQueries({
        queryKey: backupQueryKeys.list(appId),
      });
    },
  });
}

export type { Backup, BackupJob, BackupSchedule };
