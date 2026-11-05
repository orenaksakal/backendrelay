import { RefreshCw } from 'lucide-react';
import { useEffect, useState } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { RelativeTime } from '@/components/data/relative-time';
import { SimpleTable } from '@/components/data/simple-table';
import type { SimpleTableColumn } from '@/components/data/simple-table';
import { Button } from '@/components/ui/button';
import type { IndexingJob } from '@/lib/api/endpoints/apps';
import {
  jobErrorMessage,
  jobProgressText,
  jobStatusLabel,
  jobTypeLabel,
  readSchemaJobLog,
  useSchemaJobs,
  type SchemaJobLogEntry,
} from '@/lib/schema/jobs';

export interface JobsTabProps {
  appId: string;
}

interface JobRow {
  entry: SchemaJobLogEntry;
  job: IndexingJob | null;
  isLoading: boolean;
  isError: boolean;
}

/**
 * C.7 Jobs tab: the checks this browser started, newest first. Running jobs
 * refresh every second through `useSchemaJobs`; there is no list-all endpoint,
 * so a job started elsewhere shows up after it is started here or in the log of
 * that browser.
 */
export function JobsTab({ appId }: JobsTabProps) {
  const queryClient = useQueryClient();
  const [entries, setEntries] = useState<SchemaJobLogEntry[]>(() =>
    readSchemaJobLog(appId),
  );

  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect -- the job log is stored per app in localStorage
    setEntries(readSchemaJobLog(appId));
  }, [appId]);

  const rows = useSchemaJobs(appId, entries);

  const columns: ReadonlyArray<SimpleTableColumn<JobRow>> = [
    {
      id: 'attribute',
      header: 'Attribute',
      cell: ({ job, entry }) => (
        <span className="text-cell font-mono">
          {job?.attr_name ?? entry.attrName}
        </span>
      ),
    },
    {
      id: 'type',
      header: 'Type',
      cell: ({ job, entry }) => (
        <span className="text-muted-foreground text-xs">
          {jobTypeLabel(job?.job_type ?? entry.jobType)}
        </span>
      ),
    },
    {
      id: 'status',
      header: 'Status',
      cell: ({ job, isError }) =>
        isError ? (
          <span className="text-destructive text-xs">Unavailable</span>
        ) : job === null ? (
          <span className="text-muted-foreground text-xs">Loading…</span>
        ) : (
          <span className="text-xs">{jobStatusLabel(job)}</span>
        ),
    },
    {
      id: 'progress',
      header: 'Progress',
      cell: ({ job }) =>
        job === null ? (
          <span className="text-muted-foreground">—</span>
        ) : (
          <span role="status" className="text-muted-foreground text-xs">
            {jobProgressText(job)}
          </span>
        ),
    },
    {
      id: 'started',
      header: 'Started',
      cell: ({ entry }) => (
        <RelativeTime value={entry.createdAt} className="text-xs" />
      ),
    },
    {
      id: 'error',
      header: 'Error',
      cell: ({ job }) =>
        job !== null && job.job_status === 'errored' ? (
          <span className="text-destructive text-xs">
            {jobErrorMessage(job)}
          </span>
        ) : (
          <span className="text-muted-foreground">—</span>
        ),
    },
  ];

  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-muted-foreground text-sm">
          Checks started from this dashboard; running jobs refresh every second.
        </p>
        <Button
          type="button"
          variant="secondary"
          size="sm"
          onClick={() => {
            setEntries(readSchemaJobLog(appId));
            void queryClient.invalidateQueries({
              queryKey: ['dash', 'apps', appId, 'indexing-jobs'],
            });
          }}
        >
          <RefreshCw aria-hidden="true" />
          Refresh
        </Button>
      </div>

      <SimpleTable
        caption="Recent indexing jobs"
        columns={columns}
        rows={rows}
        getRowId={(row) => row.entry.id}
        loading={rows.some((row) => row.isLoading)}
        empty="No indexing jobs yet. Saving a constraint that needs checking starts one."
      />
    </div>
  );
}
