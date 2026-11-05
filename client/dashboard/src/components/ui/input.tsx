import { Input as InputPrimitive } from '@base-ui/react/input';
import type * as React from 'react';
import { cn } from '@/lib/utils';

export type InputProps = Omit<
  React.ComponentProps<typeof InputPrimitive>,
  'className'
> & { className?: string };

/** Text input. Default control height is 32px (`h-8`). */
export function Input({
  className,
  'aria-label': ariaLabel,
  'aria-labelledby': ariaLabelledBy,
  ...props
}: InputProps) {
  return (
    <InputPrimitive
      data-slot="input"
      aria-label={ariaLabel}
      // Field context adds the visible label id; an explicit aria-label must
      // win when a screen supplies a more specific name.
      aria-labelledby={ariaLabelledBy}
      className={cn(
        'border-input bg-card placeholder:text-muted-foreground focus-visible:border-ring aria-invalid:border-destructive h-8 w-full min-w-0 rounded-md border px-2.5 py-1 text-sm transition-colors disabled:cursor-not-allowed disabled:opacity-50',
        className,
      )}
      {...props}
    />
  );
}
