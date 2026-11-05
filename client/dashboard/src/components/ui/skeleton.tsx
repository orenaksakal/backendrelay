import type * as React from 'react';
import { cn } from '@/lib/utils';

export type SkeletonProps = Omit<React.ComponentProps<'div'>, 'className'> & {
  className?: string;
};

/**
 * Loading placeholder. Show it only after 150ms so fast responses do not flash
 * (B.5); composite skeleton patterns live in `components/feedback`.
 */
export function Skeleton({ className, ...props }: SkeletonProps) {
  return (
    <div
      data-slot="skeleton"
      aria-hidden="true"
      className={cn('bg-muted rounded-md', className)}
      {...props}
    />
  );
}
