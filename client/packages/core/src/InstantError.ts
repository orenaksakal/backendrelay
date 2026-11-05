export type InstantErrorHandler = (error: Error) => void;

/**
 * Sends an error to the app's `onError` hook without letting a broken hook
 * mask or replace the original failure.
 */
export function notifyError(
  handler: InstantErrorHandler | null | undefined,
  error: unknown,
): void {
  if (typeof handler !== 'function') {
    return;
  }
  const normalized =
    error instanceof Error ? error : new InstantError(String(error));
  try {
    handler(normalized);
  } catch (_e) {
    // The hook is for reporting; it must never break the SDK.
  }
}

export class InstantError extends Error {
  hint?: unknown;
  traceId?: string;

  constructor(message: string, hint?: unknown, traceId?: string) {
    super(message);
    this.hint = hint;
    if (traceId) {
      this.traceId = traceId;
    }

    const actualProto = new.target.prototype;
    if (Object.setPrototypeOf) {
      Object.setPrototypeOf(this, actualProto);
    }

    // Maintain proper stack trace for where our error was thrown
    if (Error.captureStackTrace) {
      Error.captureStackTrace(this, InstantError);
    }

    this.name = 'InstantError';
  }

  get [Symbol.toStringTag]() {
    return 'InstantError';
  }
}
