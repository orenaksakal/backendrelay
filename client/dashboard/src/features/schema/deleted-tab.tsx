import { RotateCcw } from 'lucide-react';
import { useState } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { RelativeTime } from '@/components/data/relative-time';
import { SimpleTable } from '@/components/data/simple-table';
import type { SimpleTableColumn } from '@/components/data/simple-table';
import { ErrorState } from '@/components/feedback/error-state';
import { Button } from '@/components/ui/button';
import { withTransactionTimeout, type AppInstantClient } from '@/lib/instant';
import {
  restoreAttributeOps,
  softDeletedAttributeView,
  type SoftDeletedAttributeView,
} from '@/lib/schema/attributes';
import {
  schemaQueryKeys,
  useSoftDeletedAttributes,
} from '@/lib/schema/queries';
import { showErrorToast, toastManager } from '@/lib/toast';

export interface DeletedTabProps {
  appId: string;
  client: AppInstantClient;
}

/**
 * C.7 Deleted tab: soft-deleted attributes and their restore action. The
 * server keeps them for a short grace period, and restoring is one
 * `restore-attr` step; the live schema then brings the attribute back.
 */
export function DeletedTab({ appId, client }: DeletedTabProps) {
  const query = useSoftDeletedAttributes(appId);
  const queryClient = useQueryClient();
  const [restoring, setRestoring] = useState<string | null>(null);

  const rows: SoftDeletedAttributeView[] = (query.data?.attrs ?? []).map(
    softDeletedAttributeView,
  );
  const graceDays = query.data?.['grace-period-days'];

  async function restore(attribute: SoftDeletedAttributeView): Promise<void> {
    setRestoring(attribute.id);
    try {
      await withTransactionTimeout(
        client.core._reactor.pushOps(restoreAttributeOps(attribute.id)),
      );
      toastManager.add({
        type: 'success',
        title: `${attribute.namespace}.${attribute.name} restored`,
      });
      await queryClient.invalidateQueries({
        queryKey: schemaQueryKeys.softDeleted(appId),
      });
    } catch (failure) {
      showErrorToast(failure);
    } finally {
      setRestoring(null);
    }
  }

  if (query.isError) {
    return (
      <ErrorState
        thing="deleted attributes"
        message="The server did not return the deleted attributes."
        onRetry={() => {
          void query.refetch();
        }}
      />
    );
  }

  const columns: ReadonlyArray<SimpleTableColumn<SoftDeletedAttributeView>> = [
    {
      id: 'namespace',
      header: 'Namespace',
      cell: (attribute) => (
        <span className="text-cell font-mono">{attribute.namespace}</span>
      ),
    },
    {
      id: 'name',
      header: 'Attribute',
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
      id: 'deleted',
      header: 'Deleted',
      cell: (attribute) =>
        attribute.deletedAt === undefined ? (
          <span className="text-muted-foreground">—</span>
        ) : (
          <RelativeTime value={attribute.deletedAt} className="text-xs" />
        ),
    },
    {
      id: 'restore',
      header: <span className="sr-only">Restore</span>,
      className: 'w-28',
      cell: (attribute) => (
        <Button
          type="button"
          variant="secondary"
          size="sm"
          disabled={restoring !== null}
          onClick={() => {
            void restore(attribute);
          }}
        >
          <RotateCcw aria-hidden="true" />
          {restoring === attribute.id ? 'Restoring…' : 'Restore'}
        </Button>
      ),
    },
  ];

  return (
    <div className="flex flex-col gap-3">
      <p className="text-muted-foreground text-sm">
        {graceDays === undefined
          ? 'Deleted attributes can be restored for a few days.'
          : `Deleted attributes can be restored for ${String(graceDays)} days.`}
      </p>

      <SimpleTable
        caption="Deleted attributes"
        columns={columns}
        rows={rows}
        getRowId={(attribute) => attribute.id}
        loading={query.isLoading}
        empty="No deleted attributes"
      />
    </div>
  );
}
