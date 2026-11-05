import { Menu as MenuPrimitive } from '@base-ui/react/menu';
import { Check, ChevronRight } from 'lucide-react';
import type * as React from 'react';
import { cn } from '@/lib/utils';

/**
 * Dropdown menu on Base UI Menu: arrow keys, type-ahead and Esc work out of
 * the box. Destructive items go last, in `text-destructive`, after a
 * `DropdownMenuSeparator` (B.4).
 */
export const DropdownMenu = MenuPrimitive.Root;
export const DropdownMenuTrigger = MenuPrimitive.Trigger;
export const DropdownMenuGroup = MenuPrimitive.Group;
export const DropdownMenuRadioGroup = MenuPrimitive.RadioGroup;
export const DropdownMenuSub = MenuPrimitive.SubmenuRoot;

type PartProps<T extends React.ElementType> = Omit<
  React.ComponentProps<T>,
  'className'
> & {
  className?: string;
};

export function DropdownMenuContent({
  className,
  sideOffset = 4,
  align = 'start',
  ...props
}: PartProps<typeof MenuPrimitive.Popup> & {
  align?: React.ComponentProps<typeof MenuPrimitive.Positioner>['align'];
  sideOffset?: number;
}) {
  return (
    <MenuPrimitive.Portal>
      <MenuPrimitive.Positioner
        align={align}
        sideOffset={sideOffset}
        className="z-(--z-popover) outline-none"
      >
        <MenuPrimitive.Popup
          data-slot="dropdown-menu-content"
          className={cn(
            'border-border bg-card text-foreground max-h-[var(--available-height)] min-w-44 overflow-y-auto rounded-lg border p-1 shadow-md transition-[opacity,transform] duration-[120ms] ease-out data-[ending-style]:opacity-0 data-[starting-style]:scale-[0.98] data-[starting-style]:opacity-0',
            className,
          )}
          {...props}
        />
      </MenuPrimitive.Positioner>
    </MenuPrimitive.Portal>
  );
}

export function DropdownMenuItem({
  className,
  ...props
}: PartProps<typeof MenuPrimitive.Item>) {
  return (
    <MenuPrimitive.Item
      data-slot="dropdown-menu-item"
      className={cn(
        'data-[highlighted]:bg-muted flex h-8 cursor-default items-center gap-2 rounded-md px-2 text-sm outline-none select-none data-[disabled]:pointer-events-none data-[disabled]:opacity-50 [&_svg]:size-4 [&_svg]:shrink-0',
        className,
      )}
      {...props}
    />
  );
}

export function DropdownMenuCheckboxItem({
  className,
  children,
  ...props
}: PartProps<typeof MenuPrimitive.CheckboxItem>) {
  return (
    <MenuPrimitive.CheckboxItem
      data-slot="dropdown-menu-checkbox-item"
      className={cn(
        'data-[highlighted]:bg-muted flex h-8 cursor-default items-center gap-2 rounded-md px-2 text-sm outline-none select-none data-[disabled]:pointer-events-none data-[disabled]:opacity-50',
        className,
      )}
      {...props}
    >
      <span className="flex size-4 items-center justify-center">
        <MenuPrimitive.CheckboxItemIndicator>
          <Check className="size-4" />
        </MenuPrimitive.CheckboxItemIndicator>
      </span>
      {children}
    </MenuPrimitive.CheckboxItem>
  );
}

export function DropdownMenuRadioItem({
  className,
  children,
  ...props
}: PartProps<typeof MenuPrimitive.RadioItem>) {
  return (
    <MenuPrimitive.RadioItem
      data-slot="dropdown-menu-radio-item"
      className={cn(
        'data-[highlighted]:bg-muted flex h-8 cursor-default items-center gap-2 rounded-md px-2 text-sm outline-none select-none data-[disabled]:pointer-events-none data-[disabled]:opacity-50',
        className,
      )}
      {...props}
    >
      <span className="flex size-4 items-center justify-center">
        <MenuPrimitive.RadioItemIndicator>
          <Check className="size-4" />
        </MenuPrimitive.RadioItemIndicator>
      </span>
      {children}
    </MenuPrimitive.RadioItem>
  );
}

/** Must be inside DropdownMenuGroup or DropdownMenuRadioGroup. */
export function DropdownMenuLabel({
  className,
  ...props
}: PartProps<typeof MenuPrimitive.GroupLabel>) {
  return (
    <MenuPrimitive.GroupLabel
      data-slot="dropdown-menu-label"
      className={cn(
        'text-muted-foreground px-2 py-1.5 text-xs font-medium',
        className,
      )}
      {...props}
    />
  );
}

export function DropdownMenuSeparator({
  className,
  ...props
}: PartProps<typeof MenuPrimitive.Separator>) {
  return (
    <MenuPrimitive.Separator
      data-slot="dropdown-menu-separator"
      className={cn('bg-border my-1 h-px', className)}
      {...props}
    />
  );
}

export function DropdownMenuSubTrigger({
  className,
  children,
  ...props
}: PartProps<typeof MenuPrimitive.SubmenuTrigger>) {
  return (
    <MenuPrimitive.SubmenuTrigger
      data-slot="dropdown-menu-sub-trigger"
      className={cn(
        'data-[highlighted]:bg-muted data-[popup-open]:bg-muted flex h-8 cursor-default items-center gap-2 rounded-md px-2 text-sm outline-none select-none data-[disabled]:pointer-events-none data-[disabled]:opacity-50',
        className,
      )}
      {...props}
    >
      {children}
      <ChevronRight className="ml-auto size-4" />
    </MenuPrimitive.SubmenuTrigger>
  );
}

export function DropdownMenuSubContent({
  className,
  sideOffset = 4,
  ...props
}: PartProps<typeof MenuPrimitive.Popup> & { sideOffset?: number }) {
  return (
    <MenuPrimitive.Portal>
      <MenuPrimitive.Positioner
        sideOffset={sideOffset}
        className="z-(--z-popover) outline-none"
      >
        <MenuPrimitive.Popup
          data-slot="dropdown-menu-sub-content"
          className={cn(
            'border-border bg-card text-foreground min-w-44 rounded-lg border p-1 shadow-md transition-[opacity,transform] duration-[120ms] ease-out data-[ending-style]:opacity-0 data-[starting-style]:scale-[0.98] data-[starting-style]:opacity-0',
            className,
          )}
          {...props}
        />
      </MenuPrimitive.Positioner>
    </MenuPrimitive.Portal>
  );
}

export function DropdownMenuShortcut({
  className,
  ...props
}: PartProps<'span'>) {
  return (
    <span
      data-slot="dropdown-menu-shortcut"
      className={cn(
        'text-muted-foreground ml-auto font-mono text-xs',
        className,
      )}
      {...props}
    />
  );
}
