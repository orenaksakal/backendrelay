import { useCallback, useEffect, useRef, useState } from 'react';

/** How long a copy confirmation stays visible (B.4: the icon turns for 1.5s). */
export const COPY_FEEDBACK_MS = 1500;

export type CopyStatus = 'idle' | 'copied' | 'error';

/**
 * Writes text to the clipboard. Browsers without the async clipboard API, or a
 * denied permission, report `false` so callers can announce the failure.
 */
export async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}

export interface CopyFeedback {
  status: CopyStatus;
  copy: (text: string) => Promise<boolean>;
}

/**
 * Copy status for a button: `copied` for 1.5s, then back to `idle`. Failures
 * keep `error` in the same window so the announcement is consistent.
 */
export function useCopyFeedback(resetAfterMs = COPY_FEEDBACK_MS): CopyFeedback {
  const [status, setStatus] = useState<CopyStatus>('idle');
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    return () => {
      if (timer.current) {
        clearTimeout(timer.current);
      }
    };
  }, []);

  const copy = useCallback(
    async (text: string) => {
      const succeeded = await copyText(text);
      setStatus(succeeded ? 'copied' : 'error');
      if (timer.current) {
        clearTimeout(timer.current);
      }
      timer.current = setTimeout(() => {
        setStatus('idle');
      }, resetAfterMs);
      return succeeded;
    },
    [resetAfterMs],
  );

  return { status, copy };
}

/** Live-region text for a copy status; empty when there is nothing to say. */
export function copyAnnouncement(status: CopyStatus): string {
  switch (status) {
    case 'copied':
      return 'Copied';
    case 'error':
      return "Couldn't copy";
    default:
      return '';
  }
}
