import { useEffect, useState } from 'react';
import { Skeleton } from '@/components/ui/skeleton';
import { cn } from '@/lib/utils';

/**
 * Skeletons appear only after a short delay so fast responses do not flash a
 * placeholder (B.4, B.5.6). The delay starts when the skeleton mounts.
 */
function useVisibleAfter(delayMs: number): boolean {
  const [visible, setVisible] = useState(delayMs <= 0);

  useEffect(() => {
    if (delayMs <= 0) {
      return;
    }
    const timer = setTimeout(() => {
      setVisible(true);
    }, delayMs);
    return () => {
      clearTimeout(timer);
    };
  }, [delayMs]);

  return visible;
}

export interface TableSkeletonProps {
  /** Eight rows by default, matching a typical first page (B.4). */
  rows?: number;
  columns?: number;
  /** Hide the placeholder for shorter loads than this. */
  delayMs?: number;
  className?: string;
}

/**
 * Header plus rows, sized like `SimpleTable` so the real table does not shift
 * the layout when it arrives.
 */
export function TableSkeleton({
  rows = 8,
  columns = 4,
  delayMs = 150,
  className,
}: TableSkeletonProps) {
  const visible = useVisibleAfter(delayMs);
  if (!visible) {
    return null;
  }

  return (
    <div
      data-slot="table-skeleton"
      aria-hidden="true"
      className={cn('flex flex-col gap-2', className)}
    >
      <Skeleton className="h-9 w-full" />
      {Array.from({ length: rows }, (_, row) => (
        <div key={row} className="flex items-center gap-3">
          {Array.from({ length: columns }, (_, column) => (
            <Skeleton
              key={column}
              className={cn('h-8 flex-1', column === 0 && 'max-w-16')}
            />
          ))}
        </div>
      ))}
    </div>
  );
}

export interface CardSkeletonProps {
  /** Hide the placeholder for shorter loads than this. */
  delayMs?: number;
  className?: string;
}

/** Card with a title line and body lines, matching `SectionCard`. */
export function CardSkeleton({ delayMs = 150, className }: CardSkeletonProps) {
  const visible = useVisibleAfter(delayMs);
  if (!visible) {
    return null;
  }

  return (
    <div
      data-slot="card-skeleton"
      aria-hidden="true"
      className={cn(
        'border-border bg-card flex flex-col gap-3 rounded-lg border p-4',
        className,
      )}
    >
      <Skeleton className="h-5 w-40" />
      <Skeleton className="h-4 w-full" />
      <Skeleton className="h-4 w-2/3" />
    </div>
  );
}

export interface FormSkeletonProps {
  rows?: number;
  /** Hide the placeholder for shorter loads than this. */
  delayMs?: number;
  className?: string;
}

/** Label plus control pairs, matching a settings form. */
export function FormSkeleton({
  rows = 4,
  delayMs = 150,
  className,
}: FormSkeletonProps) {
  const visible = useVisibleAfter(delayMs);
  if (!visible) {
    return null;
  }

  return (
    <div
      data-slot="form-skeleton"
      aria-hidden="true"
      className={cn('flex flex-col gap-4', className)}
    >
      {Array.from({ length: rows }, (_, row) => (
        <div key={row} className="flex flex-col gap-1.5">
          <Skeleton className="h-4 w-28" />
          <Skeleton className="h-8 w-full" />
        </div>
      ))}
    </div>
  );
}
