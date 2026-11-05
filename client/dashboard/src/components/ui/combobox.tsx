import { Combobox as ComboboxPrimitive } from '@base-ui/react/combobox';
import { Check, ChevronDown, X } from 'lucide-react';
import type * as React from 'react';
import { cn } from '@/lib/utils';

/**
 * Base UI Combobox (filterable single/multi select). Nest
 * `ComboboxInputGroup` → `ComboboxInput` + `ComboboxTrigger`, then
 * `ComboboxContent` → `ComboboxList` → `ComboboxItem`.
 */
export const Combobox = ComboboxPrimitive.Root;
export const ComboboxValue = ComboboxPrimitive.Value;
export const ComboboxGroup = ComboboxPrimitive.Group;

type PartProps<T extends React.ElementType> = Omit<
  React.ComponentProps<T>,
  'className'
> & {
  className?: string;
};

export function ComboboxInputGroup({
  className,
  ...props
}: PartProps<typeof ComboboxPrimitive.InputGroup>) {
  return (
    <ComboboxPrimitive.InputGroup
      data-slot="combobox-input-group"
      className={cn(
        'border-input bg-card focus-within:border-ring aria-invalid:border-destructive flex h-8 w-full items-center gap-1 rounded-md border pr-1 transition-colors',
        className,
      )}
      {...props}
    />
  );
}

export function ComboboxInput({
  className,
  ...props
}: PartProps<typeof ComboboxPrimitive.Input>) {
  return (
    <ComboboxPrimitive.Input
      data-slot="combobox-input"
      className={cn(
        'placeholder:text-muted-foreground h-full min-w-0 flex-1 bg-transparent px-2.5 py-1 text-sm outline-none disabled:cursor-not-allowed disabled:opacity-50',
        className,
      )}
      {...props}
    />
  );
}

export function ComboboxTrigger({
  className,
  children,
  ...props
}: PartProps<typeof ComboboxPrimitive.Trigger>) {
  return (
    <ComboboxPrimitive.Trigger
      data-slot="combobox-trigger"
      className={cn(
        'text-muted-foreground hover:bg-muted inline-flex size-6 shrink-0 items-center justify-center rounded-sm disabled:opacity-50',
        className,
      )}
      {...props}
    >
      {children ?? <ChevronDown className="size-4" />}
    </ComboboxPrimitive.Trigger>
  );
}

export function ComboboxClear({
  className,
  children,
  ...props
}: PartProps<typeof ComboboxPrimitive.Clear>) {
  return (
    <ComboboxPrimitive.Clear
      data-slot="combobox-clear"
      className={cn(
        'text-muted-foreground hover:bg-muted inline-flex size-6 shrink-0 items-center justify-center rounded-sm',
        className,
      )}
      {...props}
    >
      {children ?? <X className="size-4" />}
    </ComboboxPrimitive.Clear>
  );
}

export function ComboboxContent({
  className,
  children,
  sideOffset = 4,
  ...props
}: PartProps<typeof ComboboxPrimitive.Popup> & { sideOffset?: number }) {
  return (
    <ComboboxPrimitive.Portal>
      <ComboboxPrimitive.Positioner
        sideOffset={sideOffset}
        className="z-(--z-popover) outline-none"
      >
        <ComboboxPrimitive.Popup
          data-slot="combobox-content"
          className={cn(
            'border-border bg-card text-foreground max-h-[min(24rem,var(--available-height))] w-[var(--anchor-width)] overflow-hidden rounded-lg border shadow-md transition-[opacity,transform] duration-[120ms] ease-out data-[ending-style]:opacity-0 data-[starting-style]:scale-[0.98] data-[starting-style]:opacity-0',
            className,
          )}
          {...props}
        >
          {children}
        </ComboboxPrimitive.Popup>
      </ComboboxPrimitive.Positioner>
    </ComboboxPrimitive.Portal>
  );
}

export function ComboboxList({
  className,
  ...props
}: PartProps<typeof ComboboxPrimitive.List>) {
  return (
    <ComboboxPrimitive.List
      data-slot="combobox-list"
      className={cn('max-h-72 overflow-y-auto p-1 empty:hidden', className)}
      {...props}
    />
  );
}

export function ComboboxEmpty({
  className,
  ...props
}: PartProps<typeof ComboboxPrimitive.Empty>) {
  return (
    <ComboboxPrimitive.Empty
      data-slot="combobox-empty"
      className={cn(
        'text-muted-foreground px-2 py-6 text-center text-sm',
        className,
      )}
      {...props}
    />
  );
}

export function ComboboxItem({
  className,
  children,
  ...props
}: PartProps<typeof ComboboxPrimitive.Item>) {
  return (
    <ComboboxPrimitive.Item
      data-slot="combobox-item"
      className={cn(
        'data-[highlighted]:bg-muted flex h-8 cursor-default items-center justify-between gap-2 rounded-md px-2 text-sm outline-none select-none data-[disabled]:pointer-events-none data-[disabled]:opacity-50',
        className,
      )}
      {...props}
    >
      {children}
      <ComboboxPrimitive.ItemIndicator>
        <Check className="size-4" />
      </ComboboxPrimitive.ItemIndicator>
    </ComboboxPrimitive.Item>
  );
}

export function ComboboxGroupLabel({
  className,
  ...props
}: PartProps<typeof ComboboxPrimitive.GroupLabel>) {
  return (
    <ComboboxPrimitive.GroupLabel
      data-slot="combobox-group-label"
      className={cn(
        'text-muted-foreground px-2 py-1.5 text-xs font-medium',
        className,
      )}
      {...props}
    />
  );
}

export function ComboboxSeparator({
  className,
  ...props
}: PartProps<typeof ComboboxPrimitive.Separator>) {
  return (
    <ComboboxPrimitive.Separator
      data-slot="combobox-separator"
      className={cn('bg-border my-1 h-px', className)}
      {...props}
    />
  );
}
