import { IdChip } from '@/components/data/id-chip';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import type { IndexingJob } from '@/lib/api/endpoints/apps';
import { explorerHref } from '@/lib/explorer/url-state';
import { invalidSamples, jobErrorMessage } from '@/lib/schema/jobs';

export interface IndexingErrorDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  appId: string;
  namespace: string;
  job: IndexingJob | null;
}

/**
 * C.7: a failed constraint check lists the entities that blocked it, and each
 * one opens in the Explorer filtered to that id. Missing-required failures have
 * ids but no values, so the value column says so instead of showing `undefined`.
 */
export function IndexingErrorDialog({
  open,
  onOpenChange,
  appId,
  namespace,
  job,
}: IndexingErrorDialogProps) {
  const samples = invalidSamples(job);
  const first = samples[0];

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="md:max-w-[560px]">
        <DialogHeader>
          <DialogTitle>Indexing check failed</DialogTitle>
          <DialogDescription>{jobErrorMessage(job)}</DialogDescription>
        </DialogHeader>

        {samples.length === 0 ? (
          <p className="text-muted-foreground mt-4 text-sm">
            The server did not return any entities. Fix the data and run the
            check again.
          </p>
        ) : (
          <div className="mt-4 overflow-x-auto">
            <table className="w-full text-left text-sm">
              <caption className="sr-only">
                {`Entities that failed the check in ${namespace}`}
              </caption>
              <thead>
                <tr className="border-border border-b">
                  <th scope="col" className="py-1.5 pr-3 font-medium">
                    Entity
                  </th>
                  <th scope="col" className="py-1.5 pr-3 font-medium">
                    Value
                  </th>
                  <th scope="col" className="py-1.5 font-medium">
                    Type
                  </th>
                </tr>
              </thead>
              <tbody>
                {samples.slice(0, 10).map((sample) => (
                  <tr
                    key={sample.entityId}
                    className="border-border border-b last:border-0"
                  >
                    <td className="py-1.5 pr-3">
                      <IdChip value={sample.entityId} />
                    </td>
                    <td className="text-cell max-w-52 truncate py-1.5 pr-3 font-mono">
                      {sample.value === null
                        ? 'not set'
                        : JSON.stringify(sample.value)}
                    </td>
                    <td className="text-muted-foreground py-1.5">
                      {sample.jsonType}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}

        <DialogFooter>
          <Button
            type="button"
            variant="secondary"
            onClick={() => {
              onOpenChange(false);
            }}
          >
            Close
          </Button>
          {first === undefined ? null : (
            <Button
              render={
                <a
                  href={explorerHref(appId, namespace, { id: first.entityId })}
                  aria-label="Open in Explorer"
                  onClick={() => {
                    onOpenChange(false);
                  }}
                />
              }
            >
              Open in Explorer
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
