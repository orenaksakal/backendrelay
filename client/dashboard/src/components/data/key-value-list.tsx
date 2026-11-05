import type { ReactNode } from 'react';
import { cn } from '@/lib/utils';

export interface KeyValueItem {
  label: ReactNode;
  value: ReactNode;
}

export interface KeyValueListProps {
  items: ReadonlyArray<KeyValueItem>;
  className?: string;
}

/** Two-column definition list for metadata (B.4). */
export function KeyValueList({ items, className }: KeyValueListProps) {
  return (
    <dl
      className={cn(
        'grid grid-cols-[minmax(7rem,auto)_1fr] items-baseline gap-x-4 gap-y-2 text-sm',
        className,
      )}
    >
      {items.map((item, index) => (
        <div key={index} className="contents">
          <dt className="text-muted-foreground text-xs">{item.label}</dt>
          <dd className="min-w-0 break-words">{item.value}</dd>
        </div>
      ))}
    </dl>
  );
}
