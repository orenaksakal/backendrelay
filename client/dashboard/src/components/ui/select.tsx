import { Select as SelectPrimitive } from '@base-ui/react/select';
import { Check, ChevronDown, ChevronUp } from 'lucide-react';
import type * as React from 'react';
import { cn } from '@/lib/utils';

/**
 * Base UI Select. Use `Select`, `SelectTrigger`, `SelectValue`,
 * `SelectContent`, `SelectItem` in that nesting order; wrap the whole thing in
 * a `Field` to get the label and error wiring.
 */
export const Select = SelectPrimitive.Root;
export const SelectValue = SelectPrimitive.Value;
export const SelectGroup = SelectPrimitive.Group;

type PartProps<T extends React.ElementType> = Omit<
  React.ComponentProps<T>,
  'className'
> & {
  className?: string;
};

export function SelectTrigger({
  className,
  children,
  ...props
}: PartProps<typeof SelectPrimitive.Trigger>) {
  return (
    <SelectPrimitive.Trigger
      data-slot="select-trigger"
      className={cn(
        'border-input bg-card data-[placeholder]:text-muted-foreground data-[popup-open]:border-ring aria-invalid:border-destructive flex h-8 w-full items-center justify-between gap-2 rounded-md border px-2.5 py-1 text-sm transition-colors disabled:cursor-not-allowed disabled:opacity-50',
        className,
      )}
      {...props}
    >
      {children}
      <SelectPrimitive.Icon>
        <ChevronDown className="text-muted-foreground size-4" />
      </SelectPrimitive.Icon>
    </SelectPrimitive.Trigger>
  );
}

export function SelectContent({
  className,
  children,
  sideOffset = 4,
  ...props
}: PartProps<typeof SelectPrimitive.Popup> & {
  sideOffset?: number;
}) {
  return (
    <SelectPrimitive.Portal>
      <SelectPrimitive.Positioner
        alignItemWithTrigger={false}
        sideOffset={sideOffset}
        className="z-(--z-popover) outline-none"
      >
        <SelectPrimitive.Popup
          data-slot="select-content"
          className={cn(
            'border-border bg-card text-foreground max-h-[min(24rem,var(--available-height))] min-w-[var(--anchor-width)] overflow-hidden rounded-lg border shadow-md transition-[opacity,transform] duration-[120ms] ease-out data-[ending-style]:opacity-0 data-[starting-style]:scale-[0.98] data-[starting-style]:opacity-0',
            className,
          )}
          {...props}
        >
          <SelectPrimitive.ScrollUpArrow className="bg-card flex h-6 items-center justify-center">
            <ChevronUp className="text-muted-foreground size-4" />
          </SelectPrimitive.ScrollUpArrow>
          <SelectPrimitive.List>{children}</SelectPrimitive.List>
          <SelectPrimitive.ScrollDownArrow className="bg-card flex h-6 items-center justify-center">
            <ChevronDown className="text-muted-foreground size-4" />
          </SelectPrimitive.ScrollDownArrow>
        </SelectPrimitive.Popup>
      </SelectPrimitive.Positioner>
    </SelectPrimitive.Portal>
  );
}

export function SelectItem({
  className,
  children,
  ...props
}: PartProps<typeof SelectPrimitive.Item>) {
  return (
    <SelectPrimitive.Item
      data-slot="select-item"
      className={cn(
        'data-[highlighted]:bg-muted flex h-8 cursor-default items-center justify-between gap-2 rounded-md px-2 text-sm outline-none select-none data-[disabled]:pointer-events-none data-[disabled]:opacity-50',
        className,
      )}
      {...props}
    >
      <SelectPrimitive.ItemText>{children}</SelectPrimitive.ItemText>
      <SelectPrimitive.ItemIndicator>
        <Check className="size-4" />
      </SelectPrimitive.ItemIndicator>
    </SelectPrimitive.Item>
  );
}

export function SelectLabel({
  className,
  ...props
}: PartProps<typeof SelectPrimitive.GroupLabel>) {
  return (
    <SelectPrimitive.GroupLabel
      data-slot="select-label"
      className={cn(
        'text-muted-foreground px-2 py-1.5 text-xs font-medium',
        className,
      )}
      {...props}
    />
  );
}

export function SelectSeparator({
  className,
  ...props
}: PartProps<typeof SelectPrimitive.Separator>) {
  return (
    <SelectPrimitive.Separator
      data-slot="select-separator"
      className={cn('bg-border my-1 h-px', className)}
      {...props}
    />
  );
}
