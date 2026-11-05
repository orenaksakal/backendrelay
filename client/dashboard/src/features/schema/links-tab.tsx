import { MoreHorizontal, Plus } from 'lucide-react';
import { useState } from 'react';
import { SimpleTable } from '@/components/data/simple-table';
import type { SimpleTableColumn } from '@/components/data/simple-table';
import { TypedConfirmDialog } from '@/components/feedback/typed-confirm-dialog';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import type {
  AppInstantClient,
  AppLink,
  AppNamespace,
  InstantAttr,
} from '@/lib/instant';
import {
  deleteAttributeOps,
  linkCardinalities,
  linkCascades,
} from '@/lib/schema/attributes';
import { showErrorToast, toastManager } from '@/lib/toast';
import { LinkSheet } from './link-sheet';

export interface LinksTabProps {
  namespace: AppNamespace | null;
  namespaces: ReadonlyArray<AppNamespace>;
  rawAttrs: Record<string, InstantAttr> | null;
  client: AppInstantClient;
}

/**
 * C.7 Links tab: both labels, the cardinality on each side and whether the
 * relation cascades. Deleting a link is Tier 3 because it removes the relation
 * from every entity at once; the underlying attributes can be restored.
 */
export function LinksTab({
  namespace,
  namespaces,
  rawAttrs,
  client,
}: LinksTabProps) {
  const [sheetOpen, setSheetOpen] = useState(false);
  const [deleting, setDeleting] = useState<AppLink | null>(null);
  const [deletePending, setDeletePending] = useState(false);

  if (namespace === null) {
    return (
      <p className="text-muted-foreground py-8 text-center text-sm">
        Choose a namespace to see its links.
      </p>
    );
  }

  const links = namespace.links;
  const existingLabels = [
    ...namespace.attributes.map((attribute) => attribute.name),
    ...links.map((link) => link.label),
  ];

  const columns: ReadonlyArray<SimpleTableColumn<AppLink>> = [
    {
      id: 'forward',
      header: 'Forward label',
      cell: (link) => <span className="text-cell font-mono">{link.label}</span>,
    },
    {
      id: 'target',
      header: 'Target namespace',
      cell: (link) => (
        <span className="text-cell font-mono">{link.targetNamespace}</span>
      ),
    },
    {
      id: 'reverse',
      header: 'Reverse label',
      cell: (link) => (
        <span className="text-cell font-mono">{link.reverseLabel ?? '—'}</span>
      ),
    },
    {
      id: 'cardinality',
      header: 'Cardinality',
      cell: (link) => {
        const cardinality = linkCardinalities(link, rawAttrs?.[link.id]);
        return (
          <span className="text-muted-foreground text-xs">
            {`has-${cardinality.forward} → has-${cardinality.reverse}`}
          </span>
        );
      },
    },
    {
      id: 'on-delete',
      header: 'On delete',
      cell: (link) =>
        linkCascades(link, rawAttrs?.[link.id]) ? (
          <Badge tone="warning">Cascade</Badge>
        ) : (
          <span className="text-muted-foreground">—</span>
        ),
    },
    {
      id: 'actions',
      header: <span className="sr-only">Actions</span>,
      className: 'w-10',
      cell: (link) => (
        <DropdownMenu>
          <DropdownMenuTrigger
            render={
              <Button
                type="button"
                variant="ghost"
                size="sm"
                className="size-7 p-0"
                aria-label={`Actions for link ${link.label}`}
              />
            }
          >
            <MoreHorizontal aria-hidden="true" />
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end">
            <DropdownMenuItem
              className="text-destructive"
              onClick={() => {
                setDeleting(link);
              }}
            >
              Delete link
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      ),
    },
  ];

  async function confirmDelete(): Promise<void> {
    if (deleting === null) return;
    setDeletePending(true);
    try {
      await client.core._reactor.pushOps(deleteAttributeOps(deleting));
      toastManager.add({
        type: 'success',
        title: `Link ${deleting.label} deleted`,
      });
      setDeleting(null);
    } catch (failure) {
      showErrorToast(failure);
    } finally {
      setDeletePending(false);
    }
  }

  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-muted-foreground text-sm">
          {`${namespace.name} · ${links.length === 1 ? '1 link' : `${String(links.length)} links`}`}
        </p>
        <Button
          type="button"
          onClick={() => {
            setSheetOpen(true);
          }}
        >
          <Plus aria-hidden="true" />
          Add link
        </Button>
      </div>

      <SimpleTable
        caption={`Links from ${namespace.name}`}
        columns={columns}
        rows={links}
        getRowId={(link) => link.id}
        empty="No links yet"
      />

      {sheetOpen ? (
        <LinkSheet
          open
          onOpenChange={setSheetOpen}
          namespace={namespace}
          namespaces={namespaces}
          existingLabels={existingLabels}
          client={client}
        />
      ) : null}

      <TypedConfirmDialog
        open={deleting !== null}
        onOpenChange={(open) => {
          if (!open) setDeleting(null);
        }}
        title={`Delete link ${deleting?.label ?? ''}?`}
        description="The relation is soft-deleted with both of its attributes."
        consequences={[
          `Every ${namespace.name} entity loses its ${deleting?.label ?? 'link'} links.`,
          `The reverse label on ${deleting?.targetNamespace ?? 'the target'} disappears too.`,
          'Restore both attributes from the Deleted tab until the grace period ends.',
        ]}
        confirmText={deleting?.label ?? ''}
        confirmLabel="Delete link"
        confirming={deletePending}
        onConfirm={() => {
          void confirmDelete();
        }}
      />
    </div>
  );
}
