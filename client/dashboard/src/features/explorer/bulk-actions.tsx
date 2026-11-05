import { Download, Trash2, X } from 'lucide-react';
import { useState } from 'react';
import { ConfirmDialog } from '@/components/feedback/confirm-dialog';
import { TypedConfirmDialog } from '@/components/feedback/typed-confirm-dialog';
import { Button } from '@/components/ui/button';
import { formatCount } from '@/lib/format';

export interface BulkActionsProps {
  /** How many rows are selected. The bar is hidden at zero. */
  count: number;
  onExport: () => void;
  onDelete: () => Promise<void> | void;
  onClear: () => void;
}

/** B.5.3: more than ten entities at once needs the typed confirmation. */
const TYPED_BULK_DELETE_THRESHOLD = 10;

/**
 * C.6 bulk bar. It floats above the grid while rows are selected; Delete is
 * Tier 2, or Tier 3 (type the count) above ten entities.
 */
export function BulkActions({
  count,
  onExport,
  onDelete,
  onClear,
}: BulkActionsProps) {
  const [confirming, setConfirming] = useState(false);
  const [deleting, setDeleting] = useState(false);
  if (count === 0) return null;

  const typed = count > TYPED_BULK_DELETE_THRESHOLD;
  const noun = count === 1 ? 'entity' : 'entities';
  const description = `You are about to delete ${formatCount(count)} ${noun} from this namespace. This cannot be undone.`;

  async function remove(): Promise<void> {
    if (deleting) return;
    setDeleting(true);
    try {
      await onDelete();
      setConfirming(false);
    } finally {
      setDeleting(false);
    }
  }

  return (
    <>
      <div
        role="region"
        aria-label="Bulk actions"
        className="border-border bg-card fixed bottom-4 left-1/2 z-(--z-sticky) flex max-w-[calc(100vw-2rem)] -translate-x-1/2 flex-wrap items-center gap-2 rounded-lg border px-3 py-2 shadow-lg"
      >
        <p className="text-sm whitespace-nowrap">
          {`${formatCount(count)} selected`}
        </p>
        <Button type="button" variant="ghost" size="sm" onClick={onExport}>
          <Download aria-hidden="true" />
          Export JSON
        </Button>
        <Button
          type="button"
          variant="destructive"
          size="sm"
          onClick={() => {
            setConfirming(true);
          }}
        >
          <Trash2 aria-hidden="true" />
          Delete
        </Button>
        <Button type="button" variant="ghost" size="sm" onClick={onClear}>
          <X aria-hidden="true" />
          Clear
        </Button>
      </div>

      {typed ? (
        <TypedConfirmDialog
          open={confirming}
          onOpenChange={setConfirming}
          title={`Delete ${formatCount(count)} entities?`}
          description={description}
          consequences={[
            'Every selected entity is removed from this namespace.',
            'Linked entities stay, but the links to them are removed.',
          ]}
          confirmText={String(count)}
          confirmLabel={count === 1 ? 'Delete entity' : 'Delete entities'}
          confirming={deleting}
          onConfirm={() => {
            void remove();
          }}
        />
      ) : (
        <ConfirmDialog
          open={confirming}
          onOpenChange={setConfirming}
          title={`Delete ${formatCount(count)} ${noun}?`}
          description={description}
          confirmLabel={count === 1 ? 'Delete entity' : 'Delete entities'}
          confirming={deleting}
          onConfirm={() => {
            void remove();
          }}
        />
      )}
    </>
  );
}
