import { z } from 'zod';
import { defineEndpoint } from './types';

export const backupSchema = z.looseObject({
  id: z.string(),
  isn: z.string().nullable(),
  backup_at: z.string(),
  files_size: z.number().nullable(),
  db_size: z.number().nullable(),
  uncompressed_size: z.number().nullable(),
  description: z.string().nullable().optional(),
  expires_at: z.string().nullable().optional(),
});

export type Backup = z.infer<typeof backupSchema>;

export const backupJobSchema = z.looseObject({
  id: z.string(),
  app_id: z.string(),
  app_backup_id: z.string().nullable(),
  job_status: z.string(),
  description: z.string().nullable().optional(),
  work_estimate: z.number().nullable().optional(),
  work_completed: z.number().nullable().optional(),
  created_at: z.string(),
  updated_at: z.string(),
  done_at: z.string().nullable().optional(),
});

export type BackupJob = z.infer<typeof backupJobSchema>;

export const backupScheduleSchema = z.looseObject({
  cron: z.string(),
  enabled: z.boolean(),
  next_run_at: z.string(),
  last_enqueued_at: z.string().nullable(),
});

export type BackupSchedule = z.infer<typeof backupScheduleSchema>;

export const restoreJobSchema = z.looseObject({
  id: z.string(),
  app_id: z.string(),
  title: z.string().nullable().optional(),
  job_status: z.string(),
  progress: z.string().nullable().optional(),
  error: z.string().nullable().optional(),
  created_at: z.string(),
  done_at: z.string().nullable().optional(),
  updated_at: z.string(),
});

export type RestoreJob = z.infer<typeof restoreJobSchema>;

/** Snapshots, scheduling, jobs and owner restores (C.15, D0.4). */
export const backupEndpoints = {
  list: defineEndpoint({
    method: 'GET',
    path: '/dash/apps/:appId/backups',
    response: z.looseObject({ backups: z.array(backupSchema) }),
  }),

  create: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/backups',
    body: z.looseObject({ description: z.string().optional() }),
    response: z.looseObject({ job: backupJobSchema }),
  }),

  remove: defineEndpoint({
    method: 'DELETE',
    path: '/dash/apps/:appId/backups/:backupId',
    response: z.looseObject({ id: z.string() }),
  }),

  restore: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/backups/:backupId/restore',
    body: z.looseObject({ title: z.string().optional() }),
    response: z.looseObject({
      app: z.looseObject({ id: z.string(), title: z.string() }),
      job: z.looseObject({ id: z.string() }),
    }),
  }),

  restoreJobs: defineEndpoint({
    method: 'GET',
    path: '/dash/apps/:appId/restore-jobs',
    response: z.looseObject({ 'restore-jobs': z.array(restoreJobSchema) }),
  }),

  getSchedule: defineEndpoint({
    method: 'GET',
    path: '/dash/apps/:appId/backup-schedule',
    response: z.looseObject({ schedule: backupScheduleSchema.nullable() }),
  }),

  setSchedule: defineEndpoint({
    method: 'PUT',
    path: '/dash/apps/:appId/backup-schedule',
    body: z.looseObject({ cron: z.string(), enabled: z.boolean() }),
    response: z.looseObject({ schedule: backupScheduleSchema.nullable() }),
  }),

  jobs: defineEndpoint({
    method: 'GET',
    path: '/dash/apps/:appId/backup-jobs',
    response: z.looseObject({ jobs: z.array(backupJobSchema) }),
  }),

  job: defineEndpoint({
    method: 'GET',
    path: '/dash/apps/:appId/backup-jobs/:jobId',
    response: z.looseObject({ job: backupJobSchema.nullable() }),
  }),

  cancelJob: defineEndpoint({
    method: 'DELETE',
    path: '/dash/apps/:appId/backup-jobs/:jobId',
    response: z.looseObject({ id: z.string() }),
  }),
};
