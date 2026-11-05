import { z } from 'zod';
import type { StatusPillStatus } from '@/components/feedback/status-pill';
import type {
  Backup,
  BackupJob,
  BackupSchedule,
  RestoreJob,
} from '@/lib/api/endpoints';
import type { EffectiveRole } from '@/lib/api/endpoints/schemas';
import { isApiError } from '@/lib/api/client';
import { formatBytes, formatCount } from '@/lib/format';
import { roleRank } from '@/lib/organization/model';

/**
 * C.15 backups: the `?tab=` contract, the role gates that mirror the server,
 * the retention window, the scheduling presets and the job-state labels.
 */

export const BACKUP_TABS = ['snapshots', 'schedule', 'jobs'] as const;

export type BackupTab = (typeof BACKUP_TABS)[number];

export const BACKUP_TAB_LABELS: Record<BackupTab, string> = {
  snapshots: 'Snapshots',
  schedule: 'Schedule',
  jobs: 'Jobs',
};

export function isBackupTab(value: unknown): value is BackupTab {
  return (
    typeof value === 'string' &&
    (BACKUP_TABS as readonly string[]).includes(value)
  );
}

export const backupSearchSchema = z.object({
  tab: z.enum(BACKUP_TABS).catch('snapshots'),
});

export type BackupSearch = z.infer<typeof backupSearchSchema>;

/** Reading snapshots, jobs and the schedule needs the collaborator role. */
export function canViewBackups(role: EffectiveRole): boolean {
  return roleRank(role) >= roleRank('collaborator');
}

/** On-demand backups use the server's collaborator write check. */
export function canCreateBackup(role: EffectiveRole): boolean {
  return roleRank(role) >= roleRank('collaborator');
}

/** Deleting a snapshot is an admin action. */
export function canDeleteBackup(role: EffectiveRole): boolean {
  return roleRank(role) >= roleRank('admin');
}

/** `PUT /backup-schedule` requires the admin role. */
export function canManageBackupSchedule(role: EffectiveRole): boolean {
  return roleRank(role) >= roleRank('admin');
}

/** D0.4 restore creates a new app, so it is owner-only. */
export function canRestoreBackup(role: EffectiveRole): boolean {
  return roleRank(role) >= roleRank('owner');
}

export type BackupExpiry =
  | 'retained'
  | 'expired'
  | 'expiring'
  | 'available'
  | 'unknown';

/** Mirrors the retention window the server applies to `GET /backups`. */
export function backupExpiry(
  backup: Pick<Backup, 'expires_at'>,
  now: number = Date.now(),
): BackupExpiry {
  if (backup.expires_at === null || backup.expires_at === undefined) {
    return 'retained';
  }
  const expiry = Date.parse(backup.expires_at);
  if (!Number.isFinite(expiry)) return 'unknown';
  if (expiry <= now) return 'expired';
  return expiry - now <= 7 * 86_400_000 ? 'expiring' : 'available';
}

/** The snapshots that are still downloadable, newest first. */
export function visibleBackups(
  backups: readonly Backup[],
  now: number = Date.now(),
): Backup[] {
  return backups
    .filter((backup) => backupExpiry(backup, now) !== 'expired')
    .sort((a, b) => Date.parse(b.backup_at) - Date.parse(a.backup_at));
}

/** Human size; null means the row was recorded without a size. */
export function backupSize(bytes: number | null | undefined): string {
  if (bytes === null || bytes === undefined) return 'Unknown';
  if (!Number.isFinite(bytes) || bytes < 0) return 'Unknown';
  return formatBytes(bytes);
}

/** The best available size for a row: uncompressed payload, else parts. */
export function backupDisplaySize(backup: Backup): string {
  if (backup.uncompressed_size !== null) {
    return backupSize(backup.uncompressed_size);
  }
  if (backup.db_size !== null || backup.files_size !== null) {
    return backupSize((backup.db_size ?? 0) + (backup.files_size ?? 0));
  }
  return 'Unknown';
}

export function backupTime(value: string, utc = false): string {
  const date = new Date(value);
  if (!Number.isFinite(date.getTime())) return value;
  return utc ? formatUtcDateTime(date) : date.toLocaleString();
}

export function formatUtcDateTime(value: string | Date): string {
  const date = value instanceof Date ? value : new Date(value);
  if (!Number.isFinite(date.getTime())) return String(value);
  return `${date
    .toISOString()
    .replace('T', ' ')
    .replace(/\.\d+Z$/, '')} UTC`;
}

export function formatLocalDateTime(value: string | Date): string {
  const date = value instanceof Date ? value : new Date(value);
  if (!Number.isFinite(date.getTime())) return String(value);
  return date.toLocaleString(undefined, {
    dateStyle: 'medium',
    timeStyle: 'short',
  });
}

/** Suggested title for the app D0.4 creates from a snapshot. */
export function restoreDefaultTitle(
  appTitle: string,
  now: Date = new Date(),
): string {
  const date = now.toISOString().slice(0, 10);
  return `${appTitle} (restored ${date})`;
}

export const BACKUP_PRESETS = ['hourly', 'daily', 'weekly', 'custom'] as const;

export type BackupPreset = (typeof BACKUP_PRESETS)[number];

export const BACKUP_PRESET_LABELS: Record<BackupPreset, string> = {
  hourly: 'Hourly',
  daily: 'Daily',
  weekly: 'Weekly',
  custom: 'Custom cron',
};

export const WEEKDAY_LABELS = [
  'Sunday',
  'Monday',
  'Tuesday',
  'Wednesday',
  'Thursday',
  'Friday',
  'Saturday',
] as const;

export interface CronTiming {
  minute: number;
  hour: number;
  /** 0–6 with Sunday as 0, matching the server's cron parser. */
  weekday: number;
}

/** The fields a preset edits; custom crons keep their raw expression. */
export function cronTiming(cron: string): CronTiming {
  const fields = cron.trim().split(/\s+/);
  return {
    minute: parseField(fields[0], 0, 59, 0),
    hour: parseField(fields[1], 0, 23, 0),
    weekday: parseField(fields[4], 0, 6, 0),
  };
}

function parseField(
  field: string | undefined,
  min: number,
  max: number,
  fallback: number,
): number {
  if (field === undefined || !/^\d+$/.test(field)) return fallback;
  const value = Number(field);
  return value >= min && value <= max ? value : fallback;
}

export function presetForCron(cron: string): BackupPreset {
  const fields = cron.trim().split(/\s+/);
  if (fields.length !== 5) return 'custom';
  if (/^\d+ \* \* \* \*$/.test(cron)) return 'hourly';
  if (/^\d+ \d+ \* \* \*$/.test(cron)) return 'daily';
  if (/^\d+ \d+ \* \* [0-6]$/.test(cron)) return 'weekly';
  return 'custom';
}

export function cronForPreset(
  preset: BackupPreset,
  timing: CronTiming,
  customCron: string,
): string {
  if (preset === 'hourly') return `${String(timing.minute)} * * * *`;
  if (preset === 'daily')
    return `${String(timing.minute)} ${String(timing.hour)} * * *`;
  if (preset === 'weekly') {
    return `${String(timing.minute)} ${String(timing.hour)} * * ${String(timing.weekday)}`;
  }
  return customCron.trim();
}

export function cronDescription(cron: string): string {
  switch (presetForCron(cron)) {
    case 'hourly': {
      const timing = cronTiming(cron);
      return `Every hour at :${String(timing.minute).padStart(2, '0')} UTC`;
    }
    case 'daily':
      return `Every day at ${formatHourMinute(cron)} UTC`;
    case 'weekly': {
      const weekday = cronTiming(cron).weekday;
      return `Every ${WEEKDAY_LABELS[weekday] ?? 'Sunday'} at ${formatHourMinute(cron)} UTC`;
    }
    default:
      return 'Custom schedule (UTC)';
  }
}

function formatHourMinute(cron: string): string {
  const timing = cronTiming(cron);
  return `${String(timing.hour).padStart(2, '0')}:${String(timing.minute).padStart(2, '0')}`;
}

/**
 * The next occurrence strictly after `after`, in UTC. Mirrors the server's
 * five-field cron syntax; the server remains authoritative.
 */
export function nextBackupOccurrence(cron: string, after = Date.now()): string {
  const fields = cron.trim().split(/\s+/);
  if (fields.length !== 5 || cron.length > 128) {
    throw new Error('Enter a five-field cron expression.');
  }
  const ranges: ReadonlyArray<readonly [number, number]> = [
    [0, 59],
    [0, 23],
    [1, 31],
    [1, 12],
    [0, 6],
  ];
  const sets = fields.map((field, index) => {
    const [min, max] = ranges[index] ?? [0, 59];
    const values = new Set<number>();
    for (const part of field.split(',')) {
      if (!/^(\*|\d+(-\d+)?)(\/\d+)?$/.test(part)) {
        throw new Error(
          'Use numbers, *, ranges, lists, or / steps in cron fields.',
        );
      }
      const [base, stepText] = part.split('/');
      if (base === undefined) {
        throw new Error('Enter a five-field cron expression.');
      }
      const step = stepText === undefined ? 1 : Number(stepText);
      let start: number;
      let end: number;
      if (base === '*') {
        start = min;
        end = max;
      } else if (base.includes('-')) {
        const bounds = base.split('-').map(Number);
        const first = bounds[0];
        const second = bounds[1];
        if (first === undefined || second === undefined) {
          throw new Error('Enter a five-field cron expression.');
        }
        start = first;
        end = second;
      } else {
        start = Number(base);
        end = start;
      }
      if (
        start < min ||
        end > max ||
        start > end ||
        !Number.isSafeInteger(step) ||
        step < 1
      ) {
        throw new Error(
          `Cron field ${String(index + 1)} must use values ${String(min)}–${String(max)} and a positive step.`,
        );
      }
      for (let n = start; n <= end; n += step) values.add(n);
    }
    return values;
  });
  const [minuteSet, hourSet, daySet, monthSet, weekdaySet] = sets;
  if (!minuteSet || !hourSet || !daySet || !monthSet || !weekdaySet) {
    throw new Error('Enter a five-field cron expression.');
  }
  const start = Math.floor(after / 60_000) * 60_000 + 60_000;
  for (let i = 0; i < 400 * 24 * 60; i++) {
    const date = new Date(start + i * 60_000);
    const day = daySet.has(date.getUTCDate());
    const weekday = weekdaySet.has(date.getUTCDay());
    const calendar =
      fields[2] === '*' || fields[4] === '*' ? day && weekday : day || weekday;
    if (
      minuteSet.has(date.getUTCMinutes()) &&
      hourSet.has(date.getUTCHours()) &&
      monthSet.has(date.getUTCMonth() + 1) &&
      calendar
    ) {
      return date.toISOString();
    }
  }
  throw new Error('This cron expression has no occurrence within 400 days.');
}

/** The next `count` runs, for the schedule preview (C.15). */
export function nextBackupRuns(
  cron: string,
  after = Date.now(),
  count = 3,
): string[] {
  const runs: string[] = [];
  let cursor = after;
  for (let i = 0; i < count; i++) {
    const next = nextBackupOccurrence(cron, cursor);
    runs.push(next);
    cursor = Date.parse(next);
  }
  return runs;
}

/** The schedule form's editable shape; `null` means "not configured". */
export interface BackupScheduleDraft {
  cron: string;
  enabled: boolean;
}

export const DEFAULT_BACKUP_CRON = '0 0 * * *';

export function scheduleDraft(
  schedule: BackupSchedule | null | undefined,
): BackupScheduleDraft {
  return {
    cron: schedule?.cron ?? DEFAULT_BACKUP_CRON,
    enabled: schedule?.enabled ?? false,
  };
}

export function scheduleDraftEqual(
  a: BackupScheduleDraft,
  b: BackupScheduleDraft,
): boolean {
  return a.cron === b.cron && a.enabled === b.enabled;
}

export function createBackupErrorMessage(error: unknown): string {
  if (
    isApiError(error) &&
    (error.status === 429 || /rate.?limit/i.test(error.message))
  ) {
    return 'Up to 2 backups per 5 minutes.';
  }
  if (isApiError(error)) return error.message;
  return error instanceof Error ? error.message : 'Could not start the backup.';
}

interface JobPill {
  status: StatusPillStatus;
  label: string;
}

export function backupJobPill(job: BackupJob): JobPill {
  switch (job.job_status) {
    case 'waiting':
      return { status: 'running', label: 'Waiting' };
    case 'processing':
      return { status: 'running', label: 'Running' };
    case 'completed':
      return { status: 'active', label: 'Completed' };
    case 'errored':
      return { status: 'failed', label: 'Failed' };
    case 'cancelled':
      return { status: 'disabled', label: 'Cancelled' };
    default:
      return { status: 'disabled', label: job.job_status };
  }
}

export function restoreJobPill(job: RestoreJob): JobPill {
  switch (job.job_status) {
    case 'waiting':
      return { status: 'running', label: 'Waiting' };
    case 'processing':
      return { status: 'running', label: 'Running' };
    case 'completed':
    case 'done':
      return { status: 'active', label: 'Done' };
    case 'errored':
    case 'failed':
      return { status: 'failed', label: 'Failed' };
    case 'cancelled':
      return { status: 'disabled', label: 'Cancelled' };
    default:
      return { status: 'disabled', label: job.job_status };
  }
}

export function isActiveBackupJob(job: BackupJob): boolean {
  return job.job_status === 'waiting' || job.job_status === 'processing';
}

export function isActiveRestoreJob(job: RestoreJob): boolean {
  return job.job_status === 'waiting' || job.job_status === 'processing';
}

/** Percent complete, or null while the estimate is unknown or zero. */
export function backupJobProgress(job: BackupJob): number | null {
  const completed = job.work_completed ?? 0;
  const estimate = job.work_estimate ?? 0;
  if (estimate <= 0) return null;
  return Math.min(100, Math.round((completed / estimate) * 100));
}

export function jobDuration(
  job:
    | Pick<BackupJob, 'created_at' | 'done_at'>
    | Pick<RestoreJob, 'created_at' | 'done_at'>,
  now = Date.now(),
): string {
  const start = Date.parse(job.created_at);
  if (!Number.isFinite(start)) return '—';
  const end =
    job.done_at === null || job.done_at === undefined
      ? now
      : Date.parse(job.done_at);
  if (!Number.isFinite(end) || end < start) return '—';
  const seconds = Math.round((end - start) / 1000);
  if (seconds < 60) return `${formatCount(seconds)}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${formatCount(minutes)}m`;
  const hours = Math.floor(minutes / 60);
  return `${formatCount(hours)}h ${formatCount(minutes % 60)}m`;
}

/** `backup download <id>`, tuned for the CLI's environment override. */
export function cliDownloadCommand(
  apiBaseUrl: string,
  appId: string,
  backupId: string,
): string {
  return `INSTANT_CLI_API_URI='${apiBaseUrl}' npx instant-cli@latest backup download ${backupId} --app ${appId}`;
}

/** The archive entry names are paths; browsers save by basename. */
export function downloadNameForEntityFile(name: string): string {
  const parts = name.split('/');
  return parts[parts.length - 1] || name;
}

export function downloadNameForStorageFile(
  path: string | null,
  locationId: string,
): string {
  if (path === null || path.trim() === '') return locationId;
  const parts = path.split('/');
  return parts[parts.length - 1] || locationId;
}
