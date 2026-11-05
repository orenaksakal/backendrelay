import { queryOptions, useQuery } from '@tanstack/react-query';
import { apiFetch } from '@/lib/api/client';
import {
  appAuthEndpoints,
  appEndpoints,
  backupEndpoints,
} from '@/lib/api/endpoints';
import { statsRefetchInterval } from './overview';

/**
 * C.5 reads four per-app resources: usage totals, the polled session snapshot,
 * backups and the sender verification status.
 */
export const overviewQueryKeys = {
  usage: (appId: string) => ['dash', 'apps', appId, 'usage'] as const,
  stats: (appId: string) => ['dash', 'apps', appId, 'stats'] as const,
  backups: (appId: string) => ['dash', 'apps', appId, 'backups'] as const,
  backupSchedule: (appId: string) =>
    ['dash', 'apps', appId, 'backup-schedule'] as const,
  senderVerification: (appId: string) =>
    ['dash', 'apps', appId, 'sender-verification'] as const,
};

export function appUsageQueryOptions(appId: string) {
  return queryOptions({
    queryKey: overviewQueryKeys.usage(appId),
    queryFn: ({ signal }) =>
      apiFetch(appEndpoints.usage, { params: { appId }, signal }),
  });
}

export function appStatsQueryOptions(appId: string) {
  return queryOptions({
    queryKey: overviewQueryKeys.stats(appId),
    queryFn: ({ signal }) =>
      apiFetch(appEndpoints.stats, { params: { appId }, signal }),
    refetchInterval: statsRefetchInterval,
    refetchIntervalInBackground: false,
    // A hidden tab stops the interval; returning to the window refetches and
    // restarts it instead of waiting for another state change.
    refetchOnWindowFocus: true,
  });
}

export function appBackupsQueryOptions(appId: string) {
  return queryOptions({
    queryKey: overviewQueryKeys.backups(appId),
    queryFn: ({ signal }) =>
      apiFetch(backupEndpoints.list, { params: { appId }, signal }),
  });
}

export function appBackupScheduleQueryOptions(appId: string) {
  return queryOptions({
    queryKey: overviewQueryKeys.backupSchedule(appId),
    queryFn: ({ signal }) =>
      apiFetch(backupEndpoints.getSchedule, { params: { appId }, signal }),
  });
}

export function appSenderVerificationQueryOptions(appId: string) {
  return queryOptions({
    queryKey: overviewQueryKeys.senderVerification(appId),
    queryFn: ({ signal }) =>
      apiFetch(appAuthEndpoints.getSenderVerification, {
        params: { appId },
        signal,
      }),
  });
}

export function useAppUsage(appId: string) {
  return useQuery(appUsageQueryOptions(appId));
}

/** Polls every five seconds until the document is hidden (C.5). */
export function useAppStats(appId: string) {
  return useQuery(appStatsQueryOptions(appId));
}

export function useAppBackups(appId: string) {
  return useQuery(appBackupsQueryOptions(appId));
}

export function useAppBackupSchedule(appId: string) {
  return useQuery(appBackupScheduleQueryOptions(appId));
}

/** The endpoint requires admin, so the caller disables it for lower roles. */
export function useSenderVerification(appId: string, enabled: boolean) {
  return useQuery({
    ...appSenderVerificationQueryOptions(appId),
    enabled,
  });
}
