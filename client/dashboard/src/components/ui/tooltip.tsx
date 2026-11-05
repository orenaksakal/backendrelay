import { Tooltip as TooltipPrimitive } from '@base-ui/react/tooltip';
import type * as React from 'react';
import { cn } from '@/lib/utils';

type PartProps<T extends React.ElementType> = Omit<
  React.ComponentProps<T>,
  'className'
> & {
  className?: string;
};

/**
 * Tooltips open after 400ms on hover and immediately on keyboard focus, and
 * close on Esc (B.4). Mount one provider near the app root.
 */
export function TooltipProvider({
  delay = 400,
  closeDelay = 100,
  ...props
}: React.ComponentProps<typeof TooltipPrimitive.Provider>) {
  return (
    <TooltipPrimitive.Provider
      delay={delay}
      closeDelay={closeDelay}
      {...props}
    />
  );
}

export const Tooltip = TooltipPrimitive.Root;

export function TooltipTrigger(
  props: React.ComponentProps<typeof TooltipPrimitive.Trigger>,
) {
  return <TooltipPrimitive.Trigger {...props} />;
}

export function TooltipContent({
  className,
  side,
  sideOffset = 6,
  ...props
}: PartProps<typeof TooltipPrimitive.Popup> & {
  side?: React.ComponentProps<typeof TooltipPrimitive.Positioner>['side'];
  sideOffset?: number;
}) {
  return (
    <TooltipPrimitive.Portal>
      <TooltipPrimitive.Positioner
        side={side}
        sideOffset={sideOffset}
        className="z-(--z-popover)"
      >
        <TooltipPrimitive.Popup
          data-slot="tooltip-content"
          className={cn(
            'bg-primary text-primary-foreground rounded-md px-2 py-1 text-xs shadow-md transition-[opacity,transform] duration-[120ms] ease-out data-[ending-style]:opacity-0 data-[starting-style]:scale-[0.98] data-[starting-style]:opacity-0',
            className,
          )}
          {...props}
        />
      </TooltipPrimitive.Positioner>
    </TooltipPrimitive.Portal>
  );
}
