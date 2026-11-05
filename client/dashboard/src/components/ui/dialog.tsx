import { Dialog as DialogPrimitive } from '@base-ui/react/dialog';
import { X } from 'lucide-react';
import type * as React from 'react';
import { cn } from '@/lib/utils';

/**
 * Short-task dialog, 440px wide (B.4). Base UI returns focus to the trigger
 * and traps it while open.
 */
export const Dialog = DialogPrimitive.Root;
export const DialogTrigger = DialogPrimitive.Trigger;
export const DialogClose = DialogPrimitive.Close;

type PartProps<T extends React.ElementType> = Omit<
  React.ComponentProps<T>,
  'className'
> & {
  className?: string;
};

export function DialogContent({
  className,
  children,
  showCloseButton = true,
  ...props
}: PartProps<typeof DialogPrimitive.Popup> & { showCloseButton?: boolean }) {
  return (
    <DialogPrimitive.Portal>
      <DialogPrimitive.Backdrop
        data-slot="dialog-backdrop"
        className="bg-foreground/40 fixed inset-0 z-(--z-dialog) transition-opacity duration-[120ms] ease-out data-[ending-style]:opacity-0 data-[starting-style]:opacity-0"
      />
      <DialogPrimitive.Popup
        data-slot="dialog-content"
        className={cn(
          'border-border bg-card text-foreground fixed top-1/2 left-1/2 z-(--z-dialog) w-[calc(100%-2rem)] max-w-[440px] -translate-x-1/2 -translate-y-1/2 rounded-lg border p-4 shadow-lg transition-[opacity,transform] duration-[120ms] ease-out data-[ending-style]:opacity-0 data-[starting-style]:scale-[0.98] data-[starting-style]:opacity-0',
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

export function DialogHeader({ className, ...props }: PartProps<'div'>) {
  return (
    <div
      data-slot="dialog-header"
      className={cn('flex flex-col gap-1 pr-8', className)}
      {...props}
    />
  );
}

export function DialogFooter({ className, ...props }: PartProps<'div'>) {
  return (
    <div
      data-slot="dialog-footer"
      className={cn('mt-4 flex items-center justify-end gap-2', className)}
      {...props}
    />
  );
}

export function DialogTitle({
  className,
  ...props
}: PartProps<typeof DialogPrimitive.Title>) {
  return (
    <DialogPrimitive.Title
      data-slot="dialog-title"
      className={cn('text-section', className)}
      {...props}
    />
  );
}

export function DialogDescription({
  className,
  ...props
}: PartProps<typeof DialogPrimitive.Description>) {
  return (
    <DialogPrimitive.Description
      data-slot="dialog-description"
      className={cn('text-muted-foreground text-sm', className)}
      {...props}
    />
  );
}
