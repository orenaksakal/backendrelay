import { Separator as SeparatorPrimitive } from '@base-ui/react/separator';
import type * as React from 'react';
import { cn } from '@/lib/utils';

export type SeparatorProps = Omit<
  React.ComponentProps<typeof SeparatorPrimitive>,
  'className'
> & { className?: string };

/** Decorative divider. It is never a substitute for a heading or a border. */
export function Separator({
  className,
  orientation = 'horizontal',
  ...props
}: SeparatorProps) {
  return (
    <SeparatorPrimitive
      data-slot="separator"
      orientation={orientation}
      className={cn(
        'bg-border shrink-0 data-[orientation=horizontal]:h-px data-[orientation=horizontal]:w-full data-[orientation=vertical]:h-full data-[orientation=vertical]:w-px',
        className,
      )}
      {...props}
    />
  );
}
