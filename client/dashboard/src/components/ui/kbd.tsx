import type * as React from 'react';
import { cn } from '@/lib/utils';

export type KbdProps = Omit<React.ComponentProps<'kbd'>, 'className'> & {
  className?: string;
};

/** Shortcut key cap, for example `⌘K` or `G E`. */
export function Kbd({ className, ...props }: KbdProps) {
  return (
    <kbd
      data-slot="kbd"
      className={cn(
        'border-border bg-muted text-muted-foreground pointer-events-none inline-flex h-5 min-w-5 items-center justify-center rounded border px-1 font-mono text-xs select-none',
        className,
      )}
      {...props}
    />
  );
}

export type KbdGroupProps = Omit<React.ComponentProps<'span'>, 'className'> & {
  className?: string;
};

export function KbdGroup({ className, ...props }: KbdGroupProps) {
  return (
    <span
      data-slot="kbd-group"
      className={cn('inline-flex items-center gap-1', className)}
      {...props}
    />
  );
}
