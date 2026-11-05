import { Checkbox as CheckboxPrimitive } from '@base-ui/react/checkbox';
import { Check, Minus } from 'lucide-react';
import type * as React from 'react';
import { cn } from '@/lib/utils';

export type CheckboxProps = Omit<
  React.ComponentProps<typeof CheckboxPrimitive.Root>,
  'className'
> & { className?: string };

/**
 * Checkbox with a 24×24px hit area around a 16px box (B.3 target size), so it
 * can sit in dense table rows without shrinking the click target.
 */
export function Checkbox({ className, ...props }: CheckboxProps) {
  return (
    <CheckboxPrimitive.Root
      data-slot="checkbox"
      className={cn(
        'group flex size-6 shrink-0 cursor-default items-center justify-center rounded-md disabled:cursor-not-allowed disabled:opacity-50',
        className,
      )}
      {...props}
    >
      <span className="border-input bg-card text-primary-foreground group-data-[checked]:border-primary group-data-[checked]:bg-primary group-data-[indeterminate]:border-primary group-data-[indeterminate]:bg-primary flex size-4 items-center justify-center rounded-sm border transition-colors">
        <CheckboxPrimitive.Indicator className="flex items-center justify-center">
          <Check className="size-3.5 group-data-[indeterminate]:hidden" />
          <Minus className="hidden size-3.5 group-data-[indeterminate]:block" />
        </CheckboxPrimitive.Indicator>
      </span>
    </CheckboxPrimitive.Root>
  );
}
