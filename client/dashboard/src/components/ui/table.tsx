import type * as React from 'react';
import { cn } from '@/lib/utils';

type TableElementProps<
  T extends
    | 'table'
    | 'thead'
    | 'tbody'
    | 'tfoot'
    | 'tr'
    | 'th'
    | 'td'
    | 'caption',
> = Omit<React.ComponentProps<T>, 'className'> & { className?: string };

/**
 * Native table in a horizontal scroll container, with a sticky header.
 * Rows are 40px tall; the header row is 36px (B.3).
 */
export function Table({ className, ...props }: TableElementProps<'table'>) {
  return (
    <div
      data-slot="table-container"
      className="relative w-full overflow-x-auto"
    >
      <table
        data-slot="table"
        className={cn('w-full caption-bottom text-sm', className)}
        {...props}
      />
    </div>
  );
}

export function TableHeader({
  className,
  ...props
}: TableElementProps<'thead'>) {
  return (
    <thead
      data-slot="table-header"
      className={cn(
        'bg-card sticky top-0 z-(--z-sticky) [&_tr]:border-b',
        className,
      )}
      {...props}
    />
  );
}

export function TableBody({ className, ...props }: TableElementProps<'tbody'>) {
  return (
    <tbody
      data-slot="table-body"
      className={cn('[&_tr:last-child]:border-0', className)}
      {...props}
    />
  );
}

export function TableFooter({
  className,
  ...props
}: TableElementProps<'tfoot'>) {
  return (
    <tfoot
      data-slot="table-footer"
      className={cn(
        'border-border bg-muted/50 border-t font-medium',
        className,
      )}
      {...props}
    />
  );
}

export function TableRow({ className, ...props }: TableElementProps<'tr'>) {
  return (
    <tr
      data-slot="table-row"
      className={cn(
        'border-border hover:bg-muted/50 data-[state=selected]:bg-accent/12 border-b transition-colors',
        className,
      )}
      {...props}
    />
  );
}

export function TableHead({ className, ...props }: TableElementProps<'th'>) {
  return (
    <th
      data-slot="table-head"
      className={cn(
        'text-muted-foreground h-9 px-3 text-left align-middle text-xs font-medium whitespace-nowrap',
        className,
      )}
      {...props}
    />
  );
}

export function TableCell({ className, ...props }: TableElementProps<'td'>) {
  return (
    <td
      data-slot="table-cell"
      className={cn('text-cell h-10 px-3 py-2 align-middle', className)}
      {...props}
    />
  );
}

export function TableCaption({
  className,
  ...props
}: TableElementProps<'caption'>) {
  return (
    <caption
      data-slot="table-caption"
      className={cn('text-muted-foreground mt-3 text-xs', className)}
      {...props}
    />
  );
}
