import { useMemo, useState } from 'react';
import { Checkbox } from '@/components/ui/checkbox';
import { Button } from '@/components/ui/button';
import {
  Sheet,
  SheetBody,
  SheetContent,
  SheetDescription,
  SheetFooter,
  SheetHeader,
  SheetTitle,
} from '@/components/ui/sheet';
import { SearchInput } from '@/components/data/search-input';
import { IdChip } from '@/components/data/id-chip';
import { EmptyState } from '@/components/feedback/empty-state';
import { TableSkeleton } from '@/components/feedback/skeletons';
import { entityLabel } from '@/lib/explorer/entities';
import { useExplorerRows } from '@/lib/explorer/rows';
import type { AppInstantClient, AppLink, AppNamespace } from '@/lib/instant';
import { formatCount } from '@/lib/format';
import { Link2 } from 'lucide-react';

export interface LinkPickerSheetProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  client: AppInstantClient;
  link: AppLink;
  /** The link's target namespace, when the schema knows it. */
  target: AppNamespace | undefined;
  /** Ids already linked, so the picker cannot add the same entity twice. */
  exclude: ReadonlyArray<string>;
  onConfirm: (ids: string[]) => void;
}

/**
 * C.6 "Add link" picker: a stacked sheet that searches the target namespace
 * and adds one (cardinality one) or several (many) entities. The search uses
 * the target's first indexed string attribute, the same rule as the toolbar.
 */
export function LinkPickerSheet({
  open,
  onOpenChange,
  client,
  link,
  target,
  exclude,
  onConfirm,
}: LinkPickerSheetProps) {
  const [query, setQuery] = useState('');
  const [selected, setSelected] = useState<string[]>([]);

  const attributes = target?.attributes ?? [];
  const searchAttribute = attributes.find(
    (attribute) => attribute.type === 'string' && attribute.isIndexed,
  );
  const where = useMemo(() => {
    const text = query.trim();
    if (text === '' || searchAttribute === undefined) return {};
    return { [searchAttribute.name]: { $ilike: `%${text}%` } };
  }, [query, searchAttribute]);

  const rows = useExplorerRows({
    client,
    namespace: open ? (target?.name ?? null) : null,
    where,
    sort: null,
    limit: 50,
  });

  const excluded = new Set(exclude);
  const visible = rows.rows.filter(
    (row) => typeof row.id === 'string' && !excluded.has(row.id),
  );

  function toggle(id: string): void {
    setSelected((current) => {
      if (link.cardinality === 'one') {
        return current.includes(id) ? [] : [id];
      }
      return current.includes(id)
        ? current.filter((candidate) => candidate !== id)
        : [...current, id];
    });
  }

  function close(next: boolean): void {
    if (!next) {
      setQuery('');
      setSelected([]);
    }
    onOpenChange(next);
  }

  return (
    <Sheet open={open} onOpenChange={close}>
      <SheetContent size="sm">
        <SheetHeader>
          <SheetTitle>{`Add ${link.label}`}</SheetTitle>
          <SheetDescription>
            {`Search ${link.targetNamespace} and select ${
              link.cardinality === 'one' ? 'one entity' : 'the entities to link'
            }.`}
          </SheetDescription>
        </SheetHeader>
        <SheetBody className="flex flex-col gap-3">
          <SearchInput
            aria-label={`Search ${link.targetNamespace}`}
            placeholder={`Search ${link.targetNamespace}`}
            value={query}
            onValueChange={setQuery}
          />

          {rows.isLoading ? (
            <TableSkeleton rows={5} columns={2} />
          ) : visible.length === 0 ? (
            <EmptyState
              icon={Link2}
              title="No matching entities"
              description={
                searchAttribute === undefined
                  ? `${link.targetNamespace} has no indexed string attribute to search.`
                  : 'Try another search, or create the entity first.'
              }
            />
          ) : (
            <ul className="flex flex-col gap-1">
              {visible.map((row) => {
                const id = row.id as string;
                const checked = selected.includes(id);
                return (
                  <li
                    key={id}
                    className="flex items-center gap-2 rounded-md px-2 py-1.5 text-sm"
                  >
                    <Checkbox
                      aria-label={`Select ${entityLabel(row)}`}
                      checked={checked}
                      onCheckedChange={() => {
                        toggle(id);
                      }}
                    />
                    <span className="min-w-0 flex-1 truncate">
                      {entityLabel(row)}
                    </span>
                    <IdChip value={id} tabIndex={-1} />
                  </li>
                );
              })}
            </ul>
          )}
        </SheetBody>
        <SheetFooter className="justify-between">
          <p className="text-muted-foreground text-xs">
            {`Selected ${formatCount(selected.length)}`}
          </p>
          <div className="flex items-center gap-2">
            <Button
              type="button"
              variant="secondary"
              onClick={() => {
                close(false);
              }}
            >
              Cancel
            </Button>
            <Button
              type="button"
              disabled={selected.length === 0}
              onClick={() => {
                onConfirm(selected);
                close(false);
              }}
            >
              Add link
            </Button>
          </div>
        </SheetFooter>
      </SheetContent>
    </Sheet>
  );
}
