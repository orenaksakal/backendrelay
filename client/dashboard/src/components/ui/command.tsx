import { Command as CommandPrimitive } from 'cmdk';
import { Search } from 'lucide-react';
import type * as React from 'react';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogTitle,
} from '@/components/ui/dialog';
import { cn } from '@/lib/utils';

type PartProps<T extends React.ElementType> = Omit<
  React.ComponentProps<T>,
  'className'
> & {
  className?: string;
};

/** cmdk root, wrapped in our dialog by `CommandDialog` for the palette (B.7). */
export function Command({
  className,
  ...props
}: PartProps<typeof CommandPrimitive>) {
  return (
    <CommandPrimitive
      data-slot="command"
      className={cn(
        'bg-card text-foreground flex h-full w-full flex-col overflow-hidden rounded-lg',
        className,
      )}
      {...props}
    />
  );
}

export function CommandInput({
  className,
  ...props
}: PartProps<typeof CommandPrimitive.Input>) {
  return (
    <div
      data-slot="command-input-wrapper"
      className="border-border flex h-9 shrink-0 items-center gap-2 border-b px-3"
    >
      <Search
        className="text-muted-foreground size-4 shrink-0"
        aria-hidden="true"
      />
      <CommandPrimitive.Input
        data-slot="command-input"
        className={cn(
          'placeholder:text-muted-foreground h-full w-full bg-transparent text-sm outline-none disabled:opacity-50',
          className,
        )}
        {...props}
      />
    </div>
  );
}

export function CommandList({
  className,
  ...props
}: PartProps<typeof CommandPrimitive.List>) {
  return (
    <CommandPrimitive.List
      data-slot="command-list"
      className={cn(
        'max-h-80 overflow-x-hidden overflow-y-auto p-1',
        className,
      )}
      {...props}
    />
  );
}

export function CommandEmpty({
  className,
  ...props
}: PartProps<typeof CommandPrimitive.Empty>) {
  return (
    <CommandPrimitive.Empty
      data-slot="command-empty"
      className={cn(
        'text-muted-foreground py-6 text-center text-sm',
        className,
      )}
      {...props}
    />
  );
}

export function CommandGroup({
  className,
  ...props
}: PartProps<typeof CommandPrimitive.Group>) {
  return (
    <CommandPrimitive.Group
      data-slot="command-group"
      className={cn(
        'text-foreground [&_[cmdk-group-heading]]:text-muted-foreground overflow-hidden [&_[cmdk-group-heading]]:px-2 [&_[cmdk-group-heading]]:py-1.5 [&_[cmdk-group-heading]]:text-xs [&_[cmdk-group-heading]]:font-medium',
        className,
      )}
      {...props}
    />
  );
}

export function CommandItem({
  className,
  ...props
}: PartProps<typeof CommandPrimitive.Item>) {
  return (
    <CommandPrimitive.Item
      data-slot="command-item"
      className={cn(
        'data-[selected=true]:bg-muted flex h-8 cursor-default items-center gap-2 rounded-md px-2 text-sm outline-none select-none data-[disabled=true]:pointer-events-none data-[disabled=true]:opacity-50 [&_svg]:size-4 [&_svg]:shrink-0',
        className,
      )}
      {...props}
    />
  );
}

export function CommandSeparator({
  className,
  ...props
}: PartProps<typeof CommandPrimitive.Separator>) {
  return (
    <CommandPrimitive.Separator
      data-slot="command-separator"
      className={cn('bg-border my-1 h-px', className)}
      {...props}
    />
  );
}

export function CommandShortcut({ className, ...props }: PartProps<'span'>) {
  return (
    <span
      data-slot="command-shortcut"
      className={cn(
        'text-muted-foreground ml-auto font-mono text-xs',
        className,
      )}
      {...props}
    />
  );
}

/**
 * Command palette shell: 20% from the top, no close button, with a screen
 * reader only title so the dialog has an accessible name.
 */
export function CommandDialog({
  title = 'Command palette',
  description = 'Search pages, apps and actions',
  className,
  children,
  ...props
}: Omit<React.ComponentProps<typeof Dialog>, 'children'> & {
  title?: string;
  description?: string;
  className?: string;
  children?: React.ReactNode;
}) {
  return (
    <Dialog {...props}>
      <DialogContent
        showCloseButton={false}
        className={cn('top-[20%] translate-y-0 p-0', className)}
      >
        <DialogTitle className="sr-only">{title}</DialogTitle>
        <DialogDescription className="sr-only">{description}</DialogDescription>
        <Command>{children}</Command>
      </DialogContent>
    </Dialog>
  );
}
