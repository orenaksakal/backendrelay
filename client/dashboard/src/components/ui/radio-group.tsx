import { Radio as RadioPrimitive } from '@base-ui/react/radio';
import { RadioGroup as RadioGroupPrimitive } from '@base-ui/react/radio-group';
import type * as React from 'react';
import { cn } from '@/lib/utils';

export type RadioGroupProps = Omit<
  React.ComponentProps<typeof RadioGroupPrimitive>,
  'className'
> & { className?: string };

export function RadioGroup({ className, ...props }: RadioGroupProps) {
  return (
    <RadioGroupPrimitive
      data-slot="radio-group"
      className={cn('flex flex-col gap-2', className)}
      {...props}
    />
  );
}

export type RadioGroupItemProps = Omit<
  React.ComponentProps<typeof RadioPrimitive.Root>,
  'className'
> & { className?: string };

/** One radio option. Pair it with a `FieldLabel` inside a `Field`. */
export function RadioGroupItem({ className, ...props }: RadioGroupItemProps) {
  return (
    <RadioPrimitive.Root
      data-slot="radio-group-item"
      className={cn(
        'group flex size-6 shrink-0 cursor-default items-center justify-center rounded-full disabled:cursor-not-allowed disabled:opacity-50',
        className,
      )}
      {...props}
    >
      <span className="border-input bg-card group-data-[checked]:border-primary flex size-4 items-center justify-center rounded-full border transition-colors">
        <RadioPrimitive.Indicator className="bg-primary block size-2 rounded-full" />
      </span>
    </RadioPrimitive.Root>
  );
}
