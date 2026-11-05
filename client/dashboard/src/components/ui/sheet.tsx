import { Dialog as DialogPrimitive } from '@base-ui/react/dialog';
import { X } from 'lucide-react';
import type * as React from 'react';
import { cn } from '@/lib/utils';

/**
 * Right-side sheet on the Base UI Dialog. Widths come from B.3: `sm` 480px,
 * `md` 672px, `lg` 880px; below `md` the sheet takes the full width. Router
 * blockers and the unsaved-changes guard are wired by the composite layer.
 */
export type SheetSize = 'sm' | 'md' | 'lg';

const sheetSizes: Record<SheetSize, string> = {
  sm: 'md:max-w-[480px]',
  md: 'md:max-w-[672px]',
  lg: 'md:max-w-[880px]',
};

export const Sheet = DialogPrimitive.Root;
export const SheetTrigger = DialogPrimitive.Trigger;
export const SheetClose = DialogPrimitive.Close;

type PartProps<T extends React.ElementType> = Omit<
  React.ComponentProps<T>,
  'className'
> & {
  className?: string;
};

export function SheetContent({
  className,
  children,
  size = 'md',
  showCloseButton = true,
  ...props
}: PartProps<typeof DialogPrimitive.Popup> & {
  size?: SheetSize;
  showCloseButton?: boolean;
}) {
  return (
    <DialogPrimitive.Portal>
      <DialogPrimitive.Backdrop
        data-slot="sheet-backdrop"
        className="bg-foreground/40 fixed inset-0 z-(--z-sheet) transition-opacity duration-200 ease-out data-[ending-style]:opacity-0 data-[starting-style]:opacity-0"
      />
      <DialogPrimitive.Popup
        data-slot="sheet-content"
        className={cn(
          'border-border bg-card text-foreground fixed inset-y-0 right-0 z-(--z-sheet) flex h-full w-full flex-col border-l shadow-lg transition-transform duration-200 ease-out data-[ending-style]:translate-x-full data-[starting-style]:translate-x-full',
          sheetSizes[size],
          className,
        )}
        {...props}
      >
        {children}
        {showCloseButton ? (
          <DialogPrimitive.Close
            aria-label="Close"
            className="text-muted-foreground hover:bg-muted hover:text-foreground absolute top-3 right-3 inline-flex size-7 items-center justify-center rounded-md transition-colors"
          >
            <X className="size-4" />
          </DialogPrimitive.Close>
        ) : null}
      </DialogPrimitive.Popup>
    </DialogPrimitive.Portal>
  );
}

export function SheetHeader({ className, ...props }: PartProps<'div'>) {
  return (
    <div
      data-slot="sheet-header"
      className={cn(
        'border-border flex shrink-0 flex-col gap-1 border-b p-4 pr-12',
        className,
      )}
      {...props}
    />
  );
}

/** Scrollable middle region; the header and footer stay put. */
export function SheetBody({ className, ...props }: PartProps<'div'>) {
  return (
    <div
      data-slot="sheet-body"
      className={cn('min-h-0 flex-1 overflow-y-auto p-4', className)}
      {...props}
    />
  );
}

export function SheetFooter({ className, ...props }: PartProps<'div'>) {
  return (
    <div
      data-slot="sheet-footer"
      className={cn(
        'border-border flex shrink-0 items-center justify-end gap-2 border-t p-4',
        className,
      )}
      {...props}
    />
  );
}

export function SheetTitle({
  className,
  ...props
}: PartProps<typeof DialogPrimitive.Title>) {
  return (
    <DialogPrimitive.Title
      data-slot="sheet-title"
      className={cn('text-section', className)}
      {...props}
    />
  );
}

export function SheetDescription({
  className,
  ...props
}: PartProps<typeof DialogPrimitive.Description>) {
  return (
    <DialogPrimitive.Description
      data-slot="sheet-description"
      className={cn('text-muted-foreground text-sm', className)}
      {...props}
    />
  );
}
