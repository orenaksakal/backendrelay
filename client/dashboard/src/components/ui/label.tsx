import { Field as FieldPrimitive } from '@base-ui/react/field';
import type * as React from 'react';
import { cn } from '@/lib/utils';

export type LabelProps = Omit<
  React.ComponentProps<typeof FieldPrimitive.Label>,
  'className'
> & { className?: string };

/**
 * Standalone `<label>`. Inside a `Field`, use `FieldLabel` instead: Base UI
 * then wires `htmlFor`, `aria-describedby` and `aria-invalid` for you.
 */
export function Label({ className, ...props }: LabelProps) {
  return (
    <FieldPrimitive.Label
      data-slot="label"
      className={cn(
        'text-sm leading-none font-medium select-none peer-disabled:cursor-not-allowed peer-disabled:opacity-50',
        className,
      )}
      {...props}
    />
  );
}
