import { ContextMenu as ContextMenuPrimitive } from '@base-ui/react/context-menu';
import { Check } from 'lucide-react';
import type * as React from 'react';
import { cn } from '@/lib/utils';

/**
 * Right-click menu. `ContextMenuTrigger` takes `render` so a table row or card
 * can be the target; keyboard users open the same menu from the row actions.
 */
export const ContextMenu = ContextMenuPrimitive.Root;
export const ContextMenuTrigger = ContextMenuPrimitive.Trigger;
export const ContextMenuGroup = ContextMenuPrimitive.Group;
export const ContextMenuRadioGroup = ContextMenuPrimitive.RadioGroup;

type PartProps<T extends React.ElementType> = Omit<
  React.ComponentProps<T>,
  'className'
> & {
  className?: string;
};

export function ContextMenuContent({
  className,
  ...props
}: PartProps<typeof ContextMenuPrimitive.Popup>) {
  return (
    <ContextMenuPrimitive.Portal>
      <ContextMenuPrimitive.Positioner className="z-(--z-popover) outline-none">
        <ContextMenuPrimitive.Popup
          data-slot="context-menu-content"
          className={cn(
            'border-border bg-card text-foreground min-w-44 rounded-lg border p-1 shadow-md transition-[opacity,transform] duration-[120ms] ease-out data-[ending-style]:opacity-0 data-[starting-style]:scale-[0.98] data-[starting-style]:opacity-0',
            className,
          )}
          {...props}
        />
      </ContextMenuPrimitive.Positioner>
    </ContextMenuPrimitive.Portal>
  );
}

export function ContextMenuItem({
  className,
  ...props
}: PartProps<typeof ContextMenuPrimitive.Item>) {
  return (
    <ContextMenuPrimitive.Item
      data-slot="context-menu-item"
      className={cn(
        'data-[highlighted]:bg-muted flex h-8 cursor-default items-center gap-2 rounded-md px-2 text-sm outline-none select-none data-[disabled]:pointer-events-none data-[disabled]:opacity-50 [&_svg]:size-4 [&_svg]:shrink-0',
        className,
      )}
      {...props}
    />
  );
}

export function ContextMenuCheckboxItem({
  className,
  children,
  ...props
}: PartProps<typeof ContextMenuPrimitive.CheckboxItem>) {
  return (
    <ContextMenuPrimitive.CheckboxItem
      data-slot="context-menu-checkbox-item"
      className={cn(
        'data-[highlighted]:bg-muted flex h-8 cursor-default items-center gap-2 rounded-md px-2 text-sm outline-none select-none data-[disabled]:pointer-events-none data-[disabled]:opacity-50',
        className,
      )}
      {...props}
    >
      <span className="flex size-4 items-center justify-center">
        <ContextMenuPrimitive.CheckboxItemIndicator>
          <Check className="size-4" />
        </ContextMenuPrimitive.CheckboxItemIndicator>
      </span>
      {children}
    </ContextMenuPrimitive.CheckboxItem>
  );
}

export function ContextMenuRadioItem({
  className,
  children,
  ...props
}: PartProps<typeof ContextMenuPrimitive.RadioItem>) {
  return (
    <ContextMenuPrimitive.RadioItem
      data-slot="context-menu-radio-item"
      className={cn(
        'data-[highlighted]:bg-muted flex h-8 cursor-default items-center gap-2 rounded-md px-2 text-sm outline-none select-none data-[disabled]:pointer-events-none data-[disabled]:opacity-50',
        className,
      )}
      {...props}
    >
      <span className="flex size-4 items-center justify-center">
        <ContextMenuPrimitive.RadioItemIndicator>
          <Check className="size-4" />
        </ContextMenuPrimitive.RadioItemIndicator>
      </span>
      {children}
    </ContextMenuPrimitive.RadioItem>
  );
}

export function ContextMenuLabel({
  className,
  ...props
}: PartProps<typeof ContextMenuPrimitive.GroupLabel>) {
  return (
    <ContextMenuPrimitive.GroupLabel
      data-slot="context-menu-label"
      className={cn(
        'text-muted-foreground px-2 py-1.5 text-xs font-medium',
        className,
      )}
      {...props}
    />
  );
}

export function ContextMenuSeparator({
  className,
  ...props
}: PartProps<typeof ContextMenuPrimitive.Separator>) {
  return (
    <ContextMenuPrimitive.Separator
      data-slot="context-menu-separator"
      className={cn('bg-border my-1 h-px', className)}
      {...props}
    />
  );
}

export function ContextMenuShortcut({
  className,
  ...props
}: PartProps<'span'>) {
  return (
    <span
      data-slot="context-menu-shortcut"
      className={cn(
        'text-muted-foreground ml-auto font-mono text-xs',
        className,
      )}
      {...props}
    />
  );
}
