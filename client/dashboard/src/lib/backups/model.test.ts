import { describe, expect, it } from 'vitest';
import type { Backup, BackupJob, RestoreJob } from '@/lib/api/endpoints';
import {
  backupDisplaySize,
  backupExpiry,
  backupJobPill,
  backupJobProgress,
  cronDescription,
  cronForPreset,
  cronTiming,
  isActiveBackupJob,
  isActiveRestoreJob,
  jobDuration,
  nextBackupOccurrence,
  nextBackupRuns,
  presetForCron,
  restoreDefaultTitle,
  restoreJobPill,
  scheduleDraft,
  scheduleDraftEqual,
  visibleBackups,
} from './model';

const NOW = Date.parse('2026-09-12T17:19:34Z');

function backup(
  id: string,
  expiresAt: string | null,
  backupAt = '2026-09-07T00:00:00Z',
): Backup {
  return {
    id,
    isn: null,
    backup_at: backupAt,
    description: id,
    files_size: null,
    db_size: 0,
    uncompressed_size: null,
    expires_at: expiresAt,
  };
}

describe('backup snapshots', () => {
  it('keeps retained snapshots and hides expired ones', () => {
    const retained = backup('retained', null);
    const soon = backup('soon', '2026-09-15T00:00:00Z');
    const expired = backup('expired', '2026-09-07T00:00:00Z');
    const later = backup(
      'later',
      '2026-10-01T00:00:00Z',
      '2026-09-08T00:00:00Z',
    );

    expect(
      visibleBackups([retained, soon, expired, later], NOW).map(
        (row) => row.id,
      ),
    ).toEqual(['later', 'retained', 'soon']);
    expect(backupExpiry(soon, NOW)).toBe('expiring');
    expect(backupExpiry(expired, NOW)).toBe('expired');
    expect(backupExpiry(retained, NOW)).toBe('retained');
    expect(backupExpiry(backup('bad', 'invalid'), NOW)).toBe('unknown');
  });

  it('falls back from the uncompressed size to the recorded parts', () => {
    expect(
      backupDisplaySize({
        ...backup('size', null),
        uncompressed_size: 2_048,
      }),
    ).toBe('2 KB');
    expect(
      backupDisplaySize({
        ...backup('parts', null),
        files_size: 1_536,
        db_size: 512,
      }),
    ).toBe('2 KB');
    expect(
      backupDisplaySize({
        ...backup('unknown', null),
        files_size: null,
        db_size: null,
      }),
    ).toBe('Unknown');
  });
});

describe('backup cron presets', () => {
  it('computes exact UTC boundaries for the presets', () => {
    expect(nextBackupOccurrence('0 * * * *', NOW)).toBe(
      '2026-09-12T18:00:00.000Z',
    );
    expect(nextBackupOccurrence('0 0 * * *', NOW)).toBe(
      '2026-09-13T00:00:00.000Z',
    );
    expect(nextBackupOccurrence('0 0 * * 0', NOW)).toBe(
      '2026-09-13T00:00:00.000Z',
    );
    expect(nextBackupOccurrence('0 */6 * * *', NOW)).toBe(
      '2026-09-12T18:00:00.000Z',
    );
  });

  it('supports lists and ranges and rejects invalid input', () => {
    expect(nextBackupOccurrence('0,30 9-18 * * 1-5', NOW)).toBe(
      '2026-09-14T09:00:00.000Z',
    );
    for (const value of ['bad', '60 * * * *', '*/0 * * * *', '0 0 31 2 *']) {
      expect(() => nextBackupOccurrence(value, NOW)).toThrow();
    }
  });

  it('uses OR when both calendar day fields are restricted', () => {
    expect(nextBackupOccurrence('0 0 15 * 0', NOW)).toBe(
      '2026-09-13T00:00:00.000Z',
    );
  });

  it('lists the next runs and describes the presets', () => {
    expect(nextBackupRuns('0 0 * * *', NOW, 2)).toEqual([
      '2026-09-13T00:00:00.000Z',
      '2026-09-14T00:00:00.000Z',
    ]);
    expect(presetForCron('0 * * * *')).toBe('hourly');
    expect(presetForCron('30 3 * * *')).toBe('daily');
    expect(presetForCron('30 3 * * 1')).toBe('weekly');
    expect(presetForCron('30 3 1 * *')).toBe('custom');
    expect(cronDescription('30 3 * * 1')).toBe('Every Monday at 03:30 UTC');
    expect(cronTiming('30 3 * * 1')).toEqual({
      minute: 30,
      hour: 3,
      weekday: 1,
    });
    expect(
      cronForPreset('weekly', { minute: 30, hour: 3, weekday: 1 }, '0 0 * * *'),
    ).toBe('30 3 * * 1');
  });

  it('tracks the schedule draft against the server value', () => {
    expect(scheduleDraft(null)).toEqual({
      cron: '0 0 * * *',
      enabled: false,
    });
    expect(
      scheduleDraftEqual(
        { cron: '0 0 * * *', enabled: true },
        { cron: '0 0 * * *', enabled: true },
      ),
    ).toBe(true);
    expect(
      scheduleDraftEqual(
        { cron: '0 1 * * *', enabled: true },
        { cron: '0 0 * * *', enabled: true },
      ),
    ).toBe(false);
  });
});

describe('backup jobs', () => {
  const job: BackupJob = {
    id: 'job-1',
    app_id: 'app-1',
    app_backup_id: null,
    job_status: 'processing',
    work_estimate: 200,
    work_completed: 50,
    created_at: '2026-09-12T17:00:00Z',
    updated_at: '2026-09-12T17:01:00Z',
    done_at: null,
  };

  it('labels jobs and computes bounded progress', () => {
    expect(backupJobPill(job)).toEqual({ status: 'running', label: 'Running' });
    expect(backupJobProgress(job)).toBe(25);
    expect(backupJobProgress({ ...job, work_estimate: 0 })).toBeNull();
    expect(backupJobProgress({ ...job, work_completed: 500 })).toBe(100);
    expect(isActiveBackupJob(job)).toBe(true);
    expect(isActiveBackupJob({ ...job, job_status: 'completed' })).toBe(false);
    expect(jobDuration(job, NOW)).toBe('19m');
    expect(jobDuration({ ...job, done_at: '2026-09-12T17:00:45Z' }, NOW)).toBe(
      '45s',
    );
  });

  it('covers every restore status and the default title', () => {
    const restore = (status: string): RestoreJob => ({
      id: 'restore-1',
      app_id: 'app-2',
      title: 'copy',
      job_status: status,
      progress: null,
      error: null,
      created_at: '2026-09-12T17:00:00Z',
      done_at: null,
      updated_at: '2026-09-12T17:00:00Z',
    });

    expect(restoreJobPill(restore('waiting')).label).toBe('Waiting');
    expect(restoreJobPill(restore('processing')).label).toBe('Running');
    expect(restoreJobPill(restore('done')).label).toBe('Done');
    expect(restoreJobPill(restore('failed')).label).toBe('Failed');
    expect(restoreJobPill(restore('cancelled')).status).toBe('disabled');
    expect(isActiveRestoreJob(restore('processing'))).toBe(true);
    expect(isActiveRestoreJob(restore('done'))).toBe(false);
    expect(
      restoreDefaultTitle('todo-app', new Date('2026-10-27T00:00:00Z')),
    ).toBe('todo-app (restored 2026-10-27)');
  });
});
