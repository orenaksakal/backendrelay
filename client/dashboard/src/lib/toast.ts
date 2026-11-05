import { Toast } from '@base-ui/react/toast';
import { copyText } from './clipboard';
import { isApiError } from './api/client';

/**
 * One manager for programmatic toasts (query/mutation callbacks and route
 * loaders). The shell mounts `<ToastProvider toastManager={toastManager}>` and
 * `<Toaster />` so `useToast` and this manager show the same stack.
 */
export const toastManager = Toast.createToastManager();

/**
 * Shows a persistent, high-priority error toast (B.4). The description carries
 * the request ID so operators can quote it, and "Copy details" copies both.
 */
export function showErrorToast(error: unknown): void {
  const apiError = isApiError(error) ? error : undefined;
  const message =
    error instanceof Error && error.message.trim() !== ''
      ? error.message
      : 'Something went wrong';
  const requestId = apiError?.requestId;
  const details =
    requestId === undefined ? message : `${message} (Request ID: ${requestId})`;

  toastManager.add({
    type: 'error',
    title: message,
    description:
      requestId === undefined ? undefined : `Request ID: ${requestId}`,
    priority: 'high',
    timeout: 0,
    actionProps: {
      children: 'Copy details',
      onClick: () => {
        void copyText(details);
      },
    },
  });
}
