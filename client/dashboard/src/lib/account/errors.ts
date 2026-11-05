import { isApiError } from '@/lib/api/client';

/** Server text when it is useful; the screen's fallback otherwise. */
export function accountErrorMessage(error: unknown, fallback: string): string {
  if (isApiError(error) && error.message.trim() !== '') return error.message;
  if (error instanceof Error && error.message.trim() !== '') {
    return error.message;
  }
  return fallback;
}
