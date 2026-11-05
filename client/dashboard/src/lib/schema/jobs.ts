import { queryOptions, useQueries, useQuery } from '@tanstack/react-query';
import { apiFetch } from '@/lib/api/client';
import { appEndpoints } from '@/lib/api/endpoints';
import type { IndexingJob, IndexingJobType } from '@/lib/api/endpoints/apps';
import { formatCount } from '@/lib/format';

/**
 * C.7 indexing jobs. Saving a constraint starts a server job; the screen polls
 * it once a second while it runs and keeps a small local log so the Jobs tab can
 * list what this browser started (the API has no list-all endpoint).
 */

export const SCHEMA_JOB_LOG_PREFIX = 'br.schemaJobs.';
export const SCHEMA_JOB_LOG_LIMIT = 20;

export interface SchemaJobLogEntry {
  id: string;
  attrId: string;
  attrName: string;
  /** The server's `job_type` string; `jobTypeLabel` maps the known ones. */
  jobType: string;
  createdAt: string;
}

export function schemaJobLogKey(appId: string): string {
  return `${SCHEMA_JOB_LOG_PREFIX}${appId}`;
}

function browserLocalStorage(): Storage | null {
  if (typeof window === 'undefined') return null;
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

function isJobLogEntry(value: unknown): value is SchemaJobLogEntry {
  if (value === null || typeof value !== 'object') return false;
  const entry = value as Record<string, unknown>;
  return (
    typeof entry['id'] === 'string' &&
    typeof entry['attrId'] === 'string' &&
    typeof entry['attrName'] === 'string' &&
    typeof entry['jobType'] === 'string' &&
    typeof entry['createdAt'] === 'string'
  );
}

export function readSchemaJobLog(
  appId: string,
  storage: Storage | null = browserLocalStorage(),
): SchemaJobLogEntry[] {
  if (storage === null) return [];
  try {
    const raw = storage.getItem(schemaJobLogKey(appId));
    if (raw === null) return [];
    const parsed: unknown = JSON.parse(raw);
    return Array.isArray(parsed) ? parsed.filter(isJobLogEntry) : [];
  } catch {
    return [];
  }
}

/** The newest job first; the log is capped so localStorage stays small. */
export function appendSchemaJobLog(
  appId: string,
  entries: ReadonlyArray<SchemaJobLogEntry>,
  storage: Storage | null = browserLocalStorage(),
): SchemaJobLogEntry[] {
  const current = readSchemaJobLog(appId, storage);
  const seen = new Set<string>();
  const next = [...entries]
    .reverse()
    .concat(current)
    .filter((entry) => {
      if (seen.has(entry.id)) return false;
      seen.add(entry.id);
      return true;
    })
    .slice(0, SCHEMA_JOB_LOG_LIMIT);
  if (storage !== null) {
    try {
      storage.setItem(schemaJobLogKey(appId), JSON.stringify(next));
    } catch {
      /* A full or blocked localStorage only loses the log. */
    }
  }
  return next;
}

/* ------------------------------------------------------------------------- */
/* Job state                                                                  */
/* ------------------------------------------------------------------------- */

export function jobIsRunning(job: IndexingJob): boolean {
  return job.job_status === 'processing' || job.job_status === 'waiting';
}

export function jobIsFailed(job: IndexingJob): boolean {
  return job.job_status === 'errored';
}

export function jobStatusLabel(job: IndexingJob): string {
  switch (job.job_status) {
    case 'waiting':
      return 'Waiting';
    case 'processing':
      return 'Running';
    case 'completed':
      return 'Done';
    case 'errored':
      return 'Failed';
    case 'canceled':
      return 'Canceled';
    default:
      return job.job_status;
  }
}

const JOB_TYPE_LABELS: Record<IndexingJobType, string> = {
  'check-data-type': 'Check type',
  'remove-data-type': 'Remove type check',
  index: 'Build index',
  'remove-index': 'Remove index',
  unique: 'Check uniqueness',
  'remove-unique': 'Remove uniqueness',
  required: 'Check required',
  'remove-required': 'Remove required',
};

export function jobTypeLabel(jobType: string): string {
  return jobType in JOB_TYPE_LABELS
    ? JOB_TYPE_LABELS[jobType as IndexingJobType]
    : jobType;
}

export function jobProgress(job: IndexingJob): number | null {
  if (job.job_status === 'completed') return 100;
  if (job.job_status !== 'processing') return null;
  const total = job.work_estimate;
  if (total === null || total === undefined || total <= 0) return null;
  const done = job.work_completed ?? 0;
  return Math.max(0, Math.min(99, Math.floor((done / total) * 100)));
}

function progressVerb(jobType: string): string {
  return jobType === 'index' ? 'Indexing' : 'Checking';
}

/**
 * C.7's progress row copy: "Checking 12,400 entities… 45%". A job without an
 * estimate has not started counting yet, so it says so instead of showing 0%.
 */
export function jobProgressText(job: IndexingJob): string {
  if (job.job_status === 'waiting') return 'Waiting for worker…';
  if (job.job_status === 'completed') return 'Done';
  if (job.job_status === 'errored') return 'Failed';
  if (job.job_status === 'canceled') return 'Canceled';
  const total = job.work_estimate;
  if (total === null || total === undefined || total <= 0) {
    return 'Estimating work…';
  }
  const percent = jobProgress(job);
  return `${progressVerb(job.job_type)} ${formatCount(total)} entities… ${String(
    percent === null ? 0 : percent,
  )}%`;
}

export function jobErrorMessage(job: IndexingJob | null | undefined): string {
  if (job === null || job === undefined || job.error === null) {
    return 'The check failed.';
  }
  switch (job.error) {
    case 'missing-required-error':
      return 'Some entities do not have this attribute set.';
    case 'triple-too-large-error':
      return 'Some values are too large to index.';
    case 'invalid-triple-error':
      return `Some values cannot be stored as ${job.checked_data_type ?? 'this type'}.`;
    case 'triple-not-unique-error':
      return 'Some entities share a value, so it cannot be unique.';
    default:
      return 'The server could not finish this check.';
  }
}

export interface InvalidSample {
  entityId: string;
  value: unknown;
  jsonType: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

/**
 * The entities a failed job rejected: `invalid_triples_sample` when the server
 * sent values, otherwise the entity ids `error_data` carries for missing
 * required values.
 */
export function invalidSamples(
  job: IndexingJob | null | undefined,
): InvalidSample[] {
  if (job === null || job === undefined) return [];
  const sample: unknown = job.invalid_triples_sample;
  if (Array.isArray(sample)) {
    return sample
      .filter(isRecord)
      .map((triple) => ({
        entityId:
          typeof triple['entity_id'] === 'string' ? triple['entity_id'] : '',
        value: triple['value'],
        jsonType:
          typeof triple['json_type'] === 'string'
            ? triple['json_type']
            : 'unknown',
      }))
      .filter((triple) => triple.entityId !== '');
  }
  if (!isRecord(job.error_data)) return [];
  const ids: unknown = job.error_data['entity-ids'];
  if (!Array.isArray(ids)) return [];
  return ids.map((id) => ({
    entityId: String(id),
    value: null,
    jsonType: 'null',
  }));
}

/* ------------------------------------------------------------------------- */
/* Requests                                                                   */
/* ------------------------------------------------------------------------- */

export const schemaJobKeys = {
  job: (appId: string, jobId: string) =>
    ['dash', 'apps', appId, 'indexing-jobs', jobId] as const,
  log: (appId: string) => ['dash', 'apps', appId, 'schema-jobs'] as const,
};

export function indexingJobQueryOptions(appId: string, jobId: string) {
  return queryOptions({
    queryKey: schemaJobKeys.job(appId, jobId),
    queryFn: ({ signal }) =>
      apiFetch(appEndpoints.indexingJob, {
        params: { appId, jobId },
        signal,
      }),
    staleTime: 0,
    // C.7: a running job refreshes once a second; finished jobs stop polling.
    refetchInterval: (query) => {
      const job = query.state.data?.job;
      return job !== null && job !== undefined && jobIsRunning(job)
        ? 1_000
        : false;
    },
  });
}

/** One running job on its own; `enabled` waits until the id exists. */
export function useIndexingJob(
  appId: string,
  jobId: string | null,
  enabled = true,
) {
  return useQuery({
    ...indexingJobQueryOptions(appId, jobId ?? ''),
    enabled: enabled && jobId !== null,
  });
}

/** The Jobs tab resolves a whole local log, polling only running entries. */
export function useSchemaJobs(
  appId: string,
  entries: ReadonlyArray<SchemaJobLogEntry>,
) {
  const results = useQueries({
    queries: entries.map((entry) => indexingJobQueryOptions(appId, entry.id)),
  });
  return entries.map((entry, index) => ({
    entry,
    job: results[index]?.data?.job ?? null,
    isLoading: results[index]?.isLoading ?? false,
    isError: results[index]?.isError ?? false,
  }));
}

export async function createIndexingJob(
  appId: string,
  attrId: string,
  job: { jobType: IndexingJobType; checkedDataType?: string },
): Promise<IndexingJob> {
  const result = await apiFetch(appEndpoints.createIndexingJob, {
    params: { appId },
    body: {
      'attr-id': attrId,
      'job-type': job.jobType,
      ...(job.checkedDataType === undefined
        ? {}
        : { 'checked-data-type': job.checkedDataType }),
    },
  });
  return result.job;
}
