import type * as React from 'react';
import { cn } from '@/lib/utils';

export type TextareaProps = Omit<
  React.ComponentProps<'textarea'>,
  'className'
> & { className?: string };

/** Multi-line text input. */
export function Textarea({ className, ...props }: TextareaProps) {
  return (
    <textarea
      data-slot="textarea"
      className={cn(
        'border-input bg-card placeholder:text-muted-foreground focus-visible:border-ring aria-invalid:border-destructive min-h-20 w-full resize-y rounded-md border px-2.5 py-2 text-sm transition-colors disabled:cursor-not-allowed disabled:opacity-50',
        className,
      )}
      {...props}
    />
  );
}
