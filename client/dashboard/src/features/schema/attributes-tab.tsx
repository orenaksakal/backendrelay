import { MoreHorizontal, Plus } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import { SimpleTable } from '@/components/data/simple-table';
import type { SimpleTableColumn } from '@/components/data/simple-table';
import { TypedConfirmDialog } from '@/components/feedback/typed-confirm-dialog';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import type { IndexingJob } from '@/lib/api/endpoints/apps';
import type {
  AppAttribute,
  AppInstantClient,
  AppNamespace,
  InstantAttr,
} from '@/lib/instant';
import { deleteAttributeOps } from '@/lib/schema/attributes';
import {
  appendSchemaJobLog,
  jobIsFailed,
  jobProgress,
  jobProgressText,
  useIndexingJob,
} from '@/lib/schema/jobs';
import { showErrorToast, toastManager } from '@/lib/toast';
import { AttributeSheet } from './attribute-sheet';
import { IndexingErrorDialog } from './indexing-error-dialog';

export interface AttributesTabProps {
  appId: string;
  namespace: AppNamespace | null;
  rawAttrs: Record<string, InstantAttr> | null;
  client: AppInstantClient;
}

function ConstraintBadges({ attribute }: { attribute: AppAttribute }) {
  const badges: string[] = [];
  if (attribute.isRequired) badges.push('Required');
  if (attribute.isUnique) badges.push('Unique');
  if (attribute.isIndexed) badges.push('Indexed');
  if (badges.length === 0) {
    return <span className="text-muted-foreground">—</span>;
  }
  return (
    <div className="flex flex-wrap gap-1">
      {badges.map((badge) => (
        <Badge key={badge} tone="neutral">
          {badge}
        </Badge>
      ))}
    </div>
  );
}

/**
 * C.7 Attributes tab: one row per attribute, an action menu, and a progress row
 * for every constraint check the sheet started. "Used by" stays blank until the
 * server exposes attribute sketches.
 */
export function AttributesTab({
  appId,
  namespace,
  rawAttrs,
  client,
}: AttributesTabProps) {
  const [sheetOpen, setSheetOpen] = useState(false);
  const [editing, setEditing] = useState<AppAttribute | null>(null);
  const [deleting, setDeleting] = useState<AppAttribute | null>(null);
  const [deletePending, setDeletePending] = useState(false);
  const [jobs, setJobs] = useState<IndexingJob[]>([]);
  const [failedJob, setFailedJob] = useState<IndexingJob | null>(null);
  const [errorOpen, setErrorOpen] = useState(false);

  const attributes = namespace?.attributes ?? [];
  const existingNames = attributes.map((attribute) => attribute.name);

  function openNew(): void {
    setEditing(null);
    setSheetOpen(true);
  }

  function openEdit(attribute: AppAttribute): void {
    setEditing(attribute);
    setSheetOpen(true);
  }

  function recordJobs(started: ReadonlyArray<IndexingJob>): void {
    setJobs((current) => [...started, ...current]);
    appendSchemaJobLog(
      appId,
      started.map((job) => ({
        id: job.id,
        attrId: job.attr_id,
        attrName: job.attr_name,
        jobType: job.job_type,
        createdAt: job.created_at,
      })),
    );
  }

  async function confirmDelete(): Promise<void> {
    if (deleting === null) return;
    setDeletePending(true);
    try {
      await client.core._reactor.pushOps(deleteAttributeOps(deleting));
      toastManager.add({
        type: 'success',
        title: `${deleting.name} deleted; restore it from the Deleted tab`,
      });
      setDeleting(null);
    } catch (failure) {
      showErrorToast(failure);
    } finally {
      setDeletePending(false);
    }
  }

  if (namespace === null) {
    return (
      <p className="text-muted-foreground py-8 text-center text-sm">
        Choose a namespace to see its attributes.
      </p>
    );
  }

  const columns: ReadonlyArray<SimpleTableColumn<AppAttribute>> = [
    {
      id: 'name',
      header: 'Name',
      cell: (attribute) => (
        <span className="text-cell font-mono">{attribute.name}</span>
      ),
    },
    {
      id: 'type',
      header: 'Type',
      cell: (attribute) => (
        <span className="text-muted-foreground text-xs">{attribute.type}</span>
      ),
    },
    {
      id: 'constraints',
      header: 'Constraints',
      cell: (attribute) => <ConstraintBadges attribute={attribute} />,
    },
    {
      id: 'used-by',
      header: 'Used by',
      cell: () => null,
    },
    {
      id: 'actions',
      header: <span className="sr-only">Actions</span>,
      className: 'w-10',
      cell: (attribute) => (
        <DropdownMenu>
          <DropdownMenuTrigger
            render={
              <Button
                type="button"
                variant="ghost"
                size="sm"
                className="size-7 p-0"
                aria-label={`Actions for ${attribute.name}`}
              />
            }
          >
            <MoreHorizontal aria-hidden="true" />
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end">
            <DropdownMenuItem
              disabled={attribute.name === 'id'}
              onClick={() => {
                openEdit(attribute);
              }}
            >
              Edit
            </DropdownMenuItem>
            <DropdownMenuSeparator />
            <DropdownMenuItem
              className="text-destructive"
              disabled={attribute.name === 'id'}
              onClick={() => {
                setDeleting(attribute);
              }}
            >
              Delete
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      ),
    },
  ];

  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-muted-foreground text-sm">
          {`${namespace.name} · ${attributes.length === 1 ? '1 attribute' : `${String(attributes.length)} attributes`}`}
        </p>
        <Button
          type="button"
          onClick={() => {
            openNew();
          }}
        >
          <Plus aria-hidden="true" />
          Add attribute
        </Button>
      </div>

      <SimpleTable
        caption={`Attributes in ${namespace.name}`}
        columns={columns}
        rows={attributes}
        getRowId={(attribute) => attribute.id}
        empty="No attributes yet"
      />

      {jobs.length === 0 ? null : (
        <div
          role="region"
          aria-label="Indexing checks"
          className="border-border bg-card flex flex-col rounded-lg border"
        >
          {jobs.map((job) => (
            <ConstraintProgress
              key={job.id}
              appId={appId}
              initialJob={job}
              onFailed={(failed) => {
                setFailedJob(failed);
                setErrorOpen(true);
              }}
            />
          ))}
        </div>
      )}

      {sheetOpen ? (
        <AttributeSheet
          key={`${namespace.name}:${editing?.id ?? 'new'}`}
          open
          onOpenChange={setSheetOpen}
          appId={appId}
          namespace={namespace}
          attribute={editing}
          rawAttribute={editing === null ? undefined : rawAttrs?.[editing.id]}
          existingNames={existingNames}
          client={client}
          onJobsStarted={recordJobs}
        />
      ) : null}

      <TypedConfirmDialog
        open={deleting !== null}
        onOpenChange={(open) => {
          if (!open) setDeleting(null);
        }}
        title={`Delete ${deleting?.name ?? 'attribute'}?`}
        description="The attribute is soft-deleted, so its data can still be restored for a few days."
        consequences={[
          `Every ${namespace.name} entity loses its ${deleting?.name ?? 'attribute'} value in the app's schema.`,
          'The Deleted tab can restore it until the grace period ends.',
        ]}
        confirmText={deleting?.name ?? ''}
        confirmLabel="Delete attribute"
        confirming={deletePending}
        onConfirm={() => {
          void confirmDelete();
        }}
      />

      <IndexingErrorDialog
        open={errorOpen}
        onOpenChange={setErrorOpen}
        appId={appId}
        namespace={namespace.name}
        job={failedJob}
      />
    </div>
  );
}

interface ConstraintProgressProps {
  appId: string;
  initialJob: IndexingJob;
  onFailed: (job: IndexingJob) => void;
}

/** One row per running check, announced politely as it advances (C.7). */
function ConstraintProgress({
  appId,
  initialJob,
  onFailed,
}: ConstraintProgressProps) {
  const query = useIndexingJob(appId, initialJob.id);
  const job = query.data?.job ?? initialJob;
  const percent = jobProgress(job);
  const notified = useRef(false);

  useEffect(() => {
    if (notified.current || !jobIsFailed(job)) return;
    notified.current = true;
    onFailed(job);
  }, [job, onFailed]);

  return (
    <div className="border-border flex flex-wrap items-center gap-2 border-b px-3 py-2 text-sm last:border-0">
      <span className="text-cell font-mono">{job.attr_name}</span>
      <span
        role="status"
        aria-live="polite"
        className="text-muted-foreground min-w-0 flex-1"
      >
        {jobProgressText(job)}
      </span>
      {percent === null ? null : (
        <progress
          aria-label={`${job.attr_name} check progress`}
          max={100}
          value={percent}
          className="h-1.5 w-32"
        />
      )}
    </div>
  );
}
