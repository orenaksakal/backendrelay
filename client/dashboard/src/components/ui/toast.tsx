import {
  Toast as ToastPrimitive,
  type ToastManagerAddOptions,
} from '@base-ui/react/toast';
import { X } from 'lucide-react';
import type * as React from 'react';
import { cn } from '@/lib/utils';

/**
 * Toast provider. Success toasts close after 4s; errors pass `timeout: 0` and
 * `priority: 'high'` through `useToast` and stay until dismissed (B.4).
 */
export function ToastProvider({
  timeout = 4000,
  limit = 4,
  ...props
}: React.ComponentProps<typeof ToastPrimitive.Provider>) {
  return <ToastPrimitive.Provider timeout={timeout} limit={limit} {...props} />;
}

/**
 * Bottom-right toast stack, 360px wide (B.4). Mount inside `ToastProvider`.
 *
 * Sheets and dialogs also put their action buttons at the bottom-right, so the
 * stack is lifted above that footer band. A toast pauses its timer while the
 * pointer is over it, and without the offset it could sit on top of a footer
 * button forever and swallow every click (DASH.fix9).
 */
export function Toaster({ className }: { className?: string }) {
  const { toasts } = ToastPrimitive.useToastManager();

  return (
    <ToastPrimitive.Portal>
      <ToastPrimitive.Viewport
        data-slot="toaster"
        // The viewport stays mounted with its 16px padding, so it must not hold
        // a hit area over the sheet or dialog behind it; only a toast takes
        // pointers, and it drops them while it exits (DASH.fix6).
        className={cn(
          'pointer-events-none fixed right-0 bottom-20 z-(--z-toast) flex w-full max-w-[360px] flex-col-reverse gap-2 p-4 outline-none',
          className,
        )}
      >
        {toasts.map((toast) => (
          <ToastPrimitive.Root
            key={toast.id}
            toast={toast}
            role={toast.type === 'error' ? 'alert' : 'status'}
            // Base UI defaults to a dialog role with `aria-modal=false`, which
            // is invalid on the status/alert roles a toast should use. The
            // toast's text is the live announcement, so it needs no label.
            aria-modal={undefined}
            aria-labelledby={undefined}
            aria-describedby={undefined}
            aria-hidden={false}
            data-slot="toast"
            className={cn(
              'border-border bg-card text-foreground pointer-events-auto relative flex w-full items-start gap-2 rounded-lg border p-3 shadow-lg transition-[transform,opacity] duration-150 ease-out data-[ending-style]:pointer-events-none data-[ending-style]:opacity-0 data-[starting-style]:translate-y-2 data-[starting-style]:opacity-0',
              toast.type === 'error' && 'border-destructive/50',
              toast.type === 'success' && 'border-success/50',
            )}
          >
            <ToastPrimitive.Content className="flex w-full items-start gap-2">
              <div className="flex min-w-0 flex-1 flex-col gap-0.5">
                <ToastPrimitive.Title className="text-sm font-medium" />
                <ToastPrimitive.Description className="text-muted-foreground text-xs" />
              </div>
              <ToastPrimitive.Action className="text-accent hover:bg-muted shrink-0 rounded-md px-2 py-1 text-xs font-medium" />
              <ToastPrimitive.Close
                aria-label="Dismiss"
                aria-hidden={false}
                className="text-muted-foreground hover:bg-muted hover:text-foreground inline-flex size-6 shrink-0 items-center justify-center rounded-md transition-colors"
              >
                <X className="size-4" />
              </ToastPrimitive.Close>
            </ToastPrimitive.Content>
          </ToastPrimitive.Root>
        ))}
      </ToastPrimitive.Viewport>
    </ToastPrimitive.Portal>
  );
}

export type ToastOptions = ToastManagerAddOptions<Record<string, unknown>>;

/**
 * Adds toasts with the project defaults: success 4s, error high priority and
 * persistent. Use `toast.add({ title, description, actionProps })`.
 */
export function useToast() {
  const manager = ToastPrimitive.useToastManager();

  function add(options: ToastOptions) {
    const isError = options.type === 'error';
    return manager.add({
      priority: isError ? 'high' : 'low',
      timeout: isError ? 0 : 4000,
      ...options,
    });
  }

  return { add, close: manager.close };
}
