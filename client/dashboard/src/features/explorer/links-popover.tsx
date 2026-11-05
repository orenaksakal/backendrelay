import { Link2 } from 'lucide-react';
import { IdChip } from '@/components/data/id-chip';
import { Button } from '@/components/ui/button';
import {
  Popover,
  PopoverContent,
  PopoverDescription,
  PopoverTitle,
  PopoverTrigger,
} from '@/components/ui/popover';
import { linkedEntities } from '@/lib/explorer/entities';
import { explorerHref } from '@/lib/explorer/url-state';
import type { AppLink } from '@/lib/instant';

export interface LinksPopoverProps {
  appId: string;
  link: AppLink;
  /** The link cell value: one object for cardinality one, an array for many. */
  value: unknown;
}

/**
 * "N links" chip that lists the linked entities and can open the target
 * namespace filtered to one id (C.6). Rendering the count as text means the
 * chip is understood without the popover.
 */
export function LinksPopover({ appId, link, value }: LinksPopoverProps) {
  const entities = linkedEntities(value);
  if (entities.length === 0) return null;

  const count = entities.length;
  return (
    <Popover>
      <PopoverTrigger
        render={
          <Button
            type="button"
            variant="link"
            size="sm"
            tabIndex={-1}
            className="h-6 gap-1 px-0 text-xs"
          />
        }
      >
        <Link2 aria-hidden="true" className="size-3.5" />
        {count === 1 ? '1 link' : `${String(count)} links`}
      </PopoverTrigger>
      <PopoverContent className="w-80">
        <PopoverTitle>{link.label}</PopoverTitle>
        <PopoverDescription>
          {`Linked ${link.targetNamespace}`}
        </PopoverDescription>
        <ul className="mt-3 flex flex-col gap-2">
          {entities.map((entity) => (
            <li
              key={entity.id}
              className="flex flex-wrap items-center gap-2 text-sm"
            >
              <IdChip value={entity.id} />
              <span className="min-w-0 flex-1 truncate">{entity.label}</span>
              <a
                href={explorerHref(appId, link.targetNamespace, {
                  id: entity.id,
                })}
                className="text-accent text-xs hover:underline"
              >
                Open in Explorer
                <span className="sr-only">{` ${entity.label}`}</span>
              </a>
            </li>
          ))}
        </ul>
      </PopoverContent>
    </Popover>
  );
}
