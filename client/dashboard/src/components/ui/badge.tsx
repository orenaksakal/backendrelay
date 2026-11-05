import { cva, type VariantProps } from 'class-variance-authority';
import type * as React from 'react';
import { cn } from '@/lib/utils';

/** 20px-tall tone badge. Status colours always carry text (B.1). */
export const badgeVariants = cva(
  'inline-flex h-5 shrink-0 items-center gap-1 rounded-full px-1.5 text-xs font-medium whitespace-nowrap',
  {
    variants: {
      tone: {
        neutral: 'bg-muted text-muted-foreground',
        accent: 'bg-accent/12 text-accent',
        success: 'bg-success/12 text-success',
        warning: 'bg-warning/12 text-warning',
        destructive: 'bg-destructive/12 text-destructive',
      },
    },
    defaultVariants: { tone: 'neutral' },
  },
);

export type BadgeProps = Omit<React.ComponentProps<'span'>, 'className'> &
  VariantProps<typeof badgeVariants> & { className?: string };

export function Badge({ className, tone, ...props }: BadgeProps) {
  return (
    <span
      data-slot="badge"
      className={cn(badgeVariants({ tone }), className)}
      {...props}
    />
  );
}
