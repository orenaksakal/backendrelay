import { describe, expect, it } from 'vitest';
import type { IndexingJob } from '@/lib/api/endpoints/apps';
import {
  SCHEMA_JOB_LOG_LIMIT,
  appendSchemaJobLog,
  invalidSamples,
  jobErrorMessage,
  jobIsRunning,
  jobProgress,
  jobProgressText,
  jobStatusLabel,
  jobTypeLabel,
  readSchemaJobLog,
  type SchemaJobLogEntry,
} from './jobs';

function memoryStorage(): Storage {
  const values = new Map<string, string>();
  return {
    get length() {
      return values.size;
    },
    clear: () => {
      values.clear();
    },
    getItem: (key) => values.get(key) ?? null,
    key: (index) => [...values.keys()][index] ?? null,
    removeItem: (key) => {
      values.delete(key);
    },
    setItem: (key, value) => {
      values.set(key, value);
    },
  };
}

function job(patch: Partial<IndexingJob> = {}): IndexingJob {
  return {
    id: 'job-1',
    app_id: 'app-1',
    attr_id: 'attr-title',
    attr_name: 'todos.title',
    job_type: 'check-data-type',
    job_status: 'processing',
    work_estimate: 12_400,
    work_completed: 5_580,
    created_at: '2026-10-27T10:00:00Z',
    updated_at: '2026-10-27T10:00:01Z',
    ...patch,
  };
}

function entry(id: string): SchemaJobLogEntry {
  return {
    id,
    attrId: 'attr-title',
    attrName: 'todos.title',
    jobType: 'index',
    createdAt: '2026-10-27T10:00:00Z',
  };
}

describe('schema job log', () => {
  it('keeps the newest jobs first, deduplicated and capped', () => {
    const storage = memoryStorage();
    for (let index = 0; index < SCHEMA_JOB_LOG_LIMIT; index += 1) {
      appendSchemaJobLog('app-1', [entry(`old-${String(index)}`)], storage);
    }
    const log = appendSchemaJobLog(
      'app-1',
      [entry('old-5'), entry('new')],
      storage,
    );

    expect(log).toHaveLength(SCHEMA_JOB_LOG_LIMIT);
    expect(log[0]?.id).toBe('new');
    expect(log.filter((item) => item.id === 'old-5')).toHaveLength(1);
    expect(readSchemaJobLog('app-1', storage)).toEqual(log);
  });

  it('ignores a corrupt or foreign log', () => {
    const storage = memoryStorage();
    storage.setItem('br.schemaJobs.app-1', '{not json');
    expect(readSchemaJobLog('app-1', storage)).toEqual([]);
    storage.setItem('br.schemaJobs.app-1', JSON.stringify([{ id: 1 }]));
    expect(readSchemaJobLog('app-1', storage)).toEqual([]);
    expect(readSchemaJobLog('app-2', storage)).toEqual([]);
  });
});

describe('job progress', () => {
  it('formats the progress row copy from the work estimate', () => {
    expect(jobProgress(job())).toBe(45);
    expect(jobProgressText(job())).toBe('Checking 12,400 entities… 45%');
    expect(jobProgressText(job({ job_status: 'waiting' }))).toBe(
      'Waiting for worker…',
    );
    expect(
      jobProgressText(job({ work_estimate: null, work_completed: null })),
    ).toBe('Estimating work…');
    expect(jobProgressText(job({ job_status: 'completed' }))).toBe('Done');
    expect(jobProgress(job({ job_status: 'completed' }))).toBe(100);
  });

  it('labels statuses and job types', () => {
    expect(jobStatusLabel(job())).toBe('Running');
    expect(jobStatusLabel(job({ job_status: 'errored' }))).toBe('Failed');
    expect(jobStatusLabel(job({ job_status: 'anything' }))).toBe('anything');
    expect(jobTypeLabel('check-data-type')).toBe('Check type');
    expect(jobTypeLabel('custom')).toBe('custom');
    expect(jobIsRunning(job())).toBe(true);
    expect(jobIsRunning(job({ job_status: 'completed' }))).toBe(false);
  });

  it('explains the known failure kinds', () => {
    expect(jobErrorMessage(job({ error: 'missing-required-error' }))).toBe(
      'Some entities do not have this attribute set.',
    );
    expect(jobErrorMessage(null)).toBe('The check failed.');
    expect(jobErrorMessage(job({ error: 'other' }))).toBe(
      'The server could not finish this check.',
    );
  });

  it('reads invalid samples from tuples or entity ids', () => {
    expect(
      invalidSamples(
        job({
          invalid_triples_sample: [
            { entity_id: 'e1', value: 1, json_type: 'number' },
          ],
        }),
      ),
    ).toEqual([{ entityId: 'e1', value: 1, jsonType: 'number' }]);

    expect(
      invalidSamples(
        job({
          error: 'missing-required-error',
          error_data: { count: 2, 'entity-ids': ['e1', 'e2'] },
        }),
      ),
    ).toEqual([
      { entityId: 'e1', value: null, jsonType: 'null' },
      { entityId: 'e2', value: null, jsonType: 'null' },
    ]);
    expect(invalidSamples(null)).toEqual([]);
  });
});
