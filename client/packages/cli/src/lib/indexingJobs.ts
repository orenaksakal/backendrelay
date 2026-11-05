import { HttpClientResponse } from '@effect/platform';
import chalk from 'chalk';
import { Cause, Data, Effect, Fiber, Schema } from 'effect';
import { InstantHttpAuthed, withCommand } from './http.ts';
import { UI } from '../ui/index.ts';
import { runUIEffect } from './ui.ts';
import { link } from '../logging.ts';

type IndexingJob = {
  id: string;
  app_id: string;
  attr_name: string;
  job_type: string;
  job_status: string;
  updated_at: string;
  checked_data_type?: string;
  invalid_triples_sample?: Array<{
    entity_id: string;
    value: unknown;
    json_type: string;
  }>;
  invalid_unique_value?: unknown;
  error?: string;
  work_estimate?: number;
  work_completed?: number;
};

type IndexingJobs = {
  'group-id': string;
  jobs: IndexingJob[];
};

export class IndexingJobsError extends Data.TaggedError('IndexingJobsError')<{
  message: string;
}> {}

type InvalidTripleSample = NonNullable<
  IndexingJob['invalid_triples_sample']
>[number];

const indexingJobCompletedActionMessage = (job: IndexingJob) => {
  switch (job.job_type) {
    case 'check-data-type':
      return `setting type of ${job.attr_name} to ${job.checked_data_type}`;
    case 'remove-data-type':
      return `removing type from ${job.attr_name}`;
    case 'index':
      return `adding index to ${job.attr_name}`;
    case 'remove-index':
      return `removing index from ${job.attr_name}`;
    case 'unique':
      return `adding uniqueness constraint to ${job.attr_name}`;
    case 'remove-unique':
      return `removing uniqueness constraint from ${job.attr_name}`;
    case 'required':
      return `adding required constraint to ${job.attr_name}`;
    case 'remove-required':
      return `removing required constraint from ${job.attr_name}`;
    default:
      return `unexpected job type ${job.job_type} - please ping us on discord with this job id (${job.id})`;
  }
};

const truncate = (value: string, maxLength: number) =>
  value.length > maxLength ? `${value.slice(0, maxLength - 3)}...` : value;

const padCell = (value: string, width: number) => {
  const trimmed = value.length > width ? value.slice(0, width) : value;
  return trimmed + ' '.repeat(width - trimmed.length);
};

const indexingJobCompletedMessage = (job: IndexingJob) => {
  const actionMessage = indexingJobCompletedActionMessage(job);
  if (job.job_status === 'canceled')
    return `Canceled ${actionMessage} before it could finish.`;
  if (job.job_status === 'completed') return `Finished ${actionMessage}.`;
  if (job.job_status !== 'errored') return;
  if (!job.invalid_triples_sample?.length) return `Error ${actionMessage}.`;

  const [entityType, label] = job.attr_name.split('.');
  const samples = job.invalid_triples_sample.slice(0, 3).map((sample) => ({
    ...sample,
    value: truncate(JSON.stringify(sample.value), 32),
  }));
  const longestValue = samples.reduce(
    (length, sample) => Math.max(length, sample.value.length),
    label.length,
  );
  const urlFor = (sample: InvalidTripleSample) => {
    const params = new URLSearchParams({
      s: 'main',
      app: job.app_id,
      t: 'explorer',
      ns: entityType,
      where: JSON.stringify(['id', sample.entity_id]),
    });
    const url = new URL(
      process.env.INSTANT_CLI_DEV
        ? 'http://localhost:3000'
        : 'https://instantdb.com',
    );
    url.pathname = '/dash';
    url.search = params.toString();
    return url.toString();
  };
  const columns = [
    { header: 'namespace', width: 15, value: () => entityType },
    {
      header: 'id',
      width: 37,
      value: (sample: (typeof samples)[number]) =>
        link(urlFor(sample), sample.entity_id),
    },
    {
      header: label,
      width: longestValue + 2,
      value: (sample: (typeof samples)[number]) => sample.value,
    },
    {
      header: 'type',
      width: 8,
      value: (sample: (typeof samples)[number]) => sample.json_type,
    },
  ];

  let message = `${chalk.red('INVALID DATA')} ${actionMessage}.\n`;
  if (job.invalid_unique_value) {
    message += `  Found multiple entities with value ${truncate(JSON.stringify(job.invalid_unique_value), 64)}.\n`;
  }
  if (job.error === 'triple-too-large-error') {
    message += '  Some of the existing data is too large to index.\n';
  }
  message += '  First few examples:\n';
  message += `  ${columns.map((column) => chalk.bold(padCell(column.header, column.width))).join(' | ')}\n`;
  message += `  ${columns.map((column) => '-'.repeat(column.width)).join('-|-')}\n`;
  for (const sample of samples) {
    message += `  ${columns.map((column) => padCell(column.value(sample), column.width)).join(' | ')}\n`;
  }
  return message;
};

const jobGroupDescription = (jobs: IndexingJob[]) => {
  const actions = new Set(
    jobs.map((job) => {
      switch (job.job_type) {
        case 'check-data-type':
        case 'remove-data-type':
          return 'updating types';
        case 'index':
        case 'remove-index':
          return 'updating indexes';
        case 'unique':
        case 'remove-unique':
          return 'updating uniqueness constraints';
        case 'required':
        case 'remove-required':
          return 'making attributes required';
        default:
          return 'updating schema';
      }
    }),
  );
  const descriptions = [...actions].sort();
  if (descriptions.length === 0) return 'updating schema';
  if (descriptions.length === 1) return descriptions[0]!;
  if (descriptions.length === 2) return descriptions.join(' and ');
  return `${descriptions.slice(0, -1).join(', ')}, and ${descriptions.at(-1)}`;
};

export const waitForIndexingJobsToFinish = (
  appId: string,
  initialJobs: IndexingJobs,
) =>
  Effect.gen(function* () {
    let resolveSpinner!: () => void;
    const spinnerPromise = new Promise<void>((resolve) => {
      resolveSpinner = resolve;
    });
    const spinner = new UI.Spinner({ promise: spinnerPromise });
    const spinnerFiber = yield* Effect.fork(runUIEffect(spinner));
    const http = (yield* InstantHttpAuthed).pipe(withCommand('push'));
    const completedIds = new Set<string>();
    const errors: string[] = [];
    let jobs = initialJobs.jobs;
    let waitMs = 20;
    let lastUpdatedAt = new Date(0);

    const waitForJobs = Effect.whileLoop({
      while: () =>
        jobs.some(
          (job) =>
            job.job_status === 'waiting' || job.job_status === 'processing',
        ),
      body: () =>
        Effect.gen(function* () {
          let updated = false;
          let workEstimateTotal = 0;
          let workCompletedTotal = 0;
          for (const job of jobs) {
            const updatedAt = new Date(job.updated_at);
            if (updatedAt > lastUpdatedAt) {
              updated = true;
              lastUpdatedAt = updatedAt;
            }
            if (
              job.job_status === 'waiting' ||
              job.job_status === 'processing'
            ) {
              workEstimateTotal += job.work_estimate ?? 50000;
              workCompletedTotal += job.work_completed ?? 0;
            } else if (!completedIds.has(job.id)) {
              completedIds.add(job.id);
              const message = indexingJobCompletedMessage(job);
              if (message) {
                spinner.addMessage(message);
                if (job.job_status === 'errored') errors.push(message);
              }
            }
          }
          if (workEstimateTotal) {
            const percent = Math.floor(
              (workCompletedTotal / workEstimateTotal) * 100,
            );
            spinner.updateText(`${jobGroupDescription(jobs)} ${percent}%`);
          }
          waitMs = updated ? 1 : Math.min(10000, waitMs * 2);
          yield* Effect.sleep(waitMs);
          const response = yield* http
            .get(
              `/dash/apps/${appId}/indexing-jobs/group/${initialJobs['group-id']}`,
            )
            .pipe(
              Effect.flatMap(HttpClientResponse.schemaBodyJson(Schema.Any)),
            );
          jobs = response.jobs;
        }),
      step: () => Effect.void,
    });

    let failure: Cause.Cause<unknown> | undefined;
    yield* waitForJobs.pipe(
      Effect.catchAllCause((cause) =>
        Effect.sync(() => {
          failure = cause;
        }),
      ),
    );
    for (const job of jobs) {
      if (!completedIds.has(job.id)) {
        completedIds.add(job.id);
        const message = indexingJobCompletedMessage(job);
        if (message) {
          spinner.addMessage(message);
          if (job.job_status === 'errored') errors.push(message);
        }
      }
    }
    resolveSpinner();
    yield* Fiber.join(spinnerFiber).pipe(Effect.ignore);
    if (failure) return yield* Effect.failCause(failure);
    if (errors.length) {
      for (const error of errors) {
        yield* Effect.log(error);
      }
      return yield* new IndexingJobsError({
        message: 'Some steps failed while updating schema.',
      });
    }
  });
