import { Check, Copy } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { copyAnnouncement, useCopyFeedback } from '@/lib/clipboard';
import { cn } from '@/lib/utils';

export interface CopyButtonProps {
  /** Text written to the clipboard. */
  value: string;
  /** Visible label; icon-only when omitted. */
  label?: string;
  /** Accessible name override; defaults to "Copy" or "Copy {label}". */
  'aria-label'?: string;
  size?: 'sm' | 'md';
  className?: string;
}

/**
 * Copies a value, swaps the icon for a check for 1.5s, and announces "Copied"
 * through a persistent `role="status"` region (B.4).
 */
export function CopyButton({
  value,
  label,
  size = 'sm',
  className,
  'aria-label': ariaLabel,
}: CopyButtonProps) {
  const { status, copy } = useCopyFeedback();

  return (
    <>
      <Button
        type="button"
        variant="ghost"
        size={size}
        aria-label={ariaLabel ?? (label ? `Copy ${label}` : 'Copy')}
        className={cn(
          label ? undefined : size === 'md' ? 'size-8 p-0' : 'size-7 p-0',
          className,
        )}
        onClick={() => {
          void copy(value);
        }}
      >
        {status === 'copied' ? (
          <Check className="text-success size-4" aria-hidden="true" />
        ) : (
          <Copy className="size-4" aria-hidden="true" />
        )}
        {label ? <span>{label}</span> : null}
      </Button>
      <span role="status" className="sr-only">
        {copyAnnouncement(status)}
      </span>
    </>
  );
}
