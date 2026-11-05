import { ScrollArea as ScrollAreaPrimitive } from '@base-ui/react/scroll-area';
import type * as React from 'react';
import { cn } from '@/lib/utils';

export type ScrollAreaProps = Omit<
  React.ComponentProps<typeof ScrollAreaPrimitive.Root>,
  'className'
> & { className?: string };

/** Custom scrollbars that match the theme, for panes and sheet bodies. */
export function ScrollArea({ className, children, ...props }: ScrollAreaProps) {
  return (
    <ScrollAreaPrimitive.Root
      data-slot="scroll-area"
      className={cn('relative overflow-hidden', className)}
      {...props}
    >
      <ScrollAreaPrimitive.Viewport className="size-full outline-none">
        {children}
      </ScrollAreaPrimitive.Viewport>
      <ScrollAreaPrimitive.Scrollbar
        orientation="vertical"
        className="flex w-2.5 touch-none p-0.5 select-none"
      >
        <ScrollAreaPrimitive.Thumb className="bg-input flex-1 rounded-full" />
      </ScrollAreaPrimitive.Scrollbar>
      <ScrollAreaPrimitive.Scrollbar
        orientation="horizontal"
        className="flex h-2.5 touch-none flex-col p-0.5 select-none"
      >
        <ScrollAreaPrimitive.Thumb className="bg-input flex-1 rounded-full" />
      </ScrollAreaPrimitive.Scrollbar>
      <ScrollAreaPrimitive.Corner className="bg-transparent" />
    </ScrollAreaPrimitive.Root>
  );
}
