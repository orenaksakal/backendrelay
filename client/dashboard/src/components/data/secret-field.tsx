import { Eye, EyeOff } from 'lucide-react';
import { useEffect, useState, type ReactNode } from 'react';
import { CopyButton } from '@/components/data/copy-button';
import { TypedConfirmDialog } from '@/components/feedback/typed-confirm-dialog';
import { Button } from '@/components/ui/button';
import { cn } from '@/lib/utils';

export interface SecretFieldProps {
  /** The full secret. Copy always uses the whole value. */
  value: string;
  /** Characters shown before the dots while hidden. */
  prefixLength?: number;
  /** Auto-hide delay after a reveal (B.4: 30s). */
  hideAfterMs?: number;
  /** Adds a Rotate action behind a Tier 3 confirmation. */
  onRotate?: () => void;
  rotateConfirmText?: string;
  rotateConsequences?: ReadonlyArray<ReactNode>;
  className?: string;
}

/**
 * `a1b2c3••••••••` with Reveal, Copy and an optional Rotate action. A revealed
 * secret hides itself again after 30s so it does not stay on screen (B.4).
 */
export function SecretField({
  value,
  prefixLength = 6,
  hideAfterMs = 30_000,
  onRotate,
  rotateConfirmText = 'rotate',
  rotateConsequences,
  className,
}: SecretFieldProps) {
  const [revealed, setRevealed] = useState(false);
  const [rotateOpen, setRotateOpen] = useState(false);

  useEffect(() => {
    if (!revealed) {
      return;
    }
    const timer = setTimeout(() => {
      setRevealed(false);
    }, hideAfterMs);
    return () => {
      clearTimeout(timer);
    };
  }, [revealed, hideAfterMs]);

  const display = revealed
    ? value
    : `${value.slice(0, prefixLength)}${'•'.repeat(8)}`;

  return (
    <div className={cn('flex flex-wrap items-center gap-2', className)}>
      <span
        data-revealed={revealed}
        className="bg-muted text-cell min-w-0 rounded-md px-2 py-1 font-mono break-all"
      >
        {display}
      </span>
      <Button
        type="button"
        variant="ghost"
        size="sm"
        aria-label={revealed ? 'Hide secret' : 'Show secret'}
        aria-pressed={revealed}
        onClick={() => {
          setRevealed((current) => !current);
        }}
      >
        {revealed ? (
          <EyeOff className="size-4" aria-hidden="true" />
        ) : (
          <Eye className="size-4" aria-hidden="true" />
        )}
      </Button>
      <CopyButton value={value} aria-label="Copy secret" />
      {onRotate ? (
        <Button
          type="button"
          variant="secondary"
          size="sm"
          onClick={() => {
            setRotateOpen(true);
          }}
        >
          Rotate
        </Button>
      ) : null}
      {onRotate ? (
        <TypedConfirmDialog
          open={rotateOpen}
          onOpenChange={setRotateOpen}
          title="Rotate this secret?"
          description="Every server that uses the current value stops working until you update it."
          consequences={
            rotateConsequences ?? [
              'The current value stops working immediately.',
              'You see the new value once, on this screen.',
            ]
          }
          confirmText={rotateConfirmText}
          confirmLabel="Rotate"
          onConfirm={() => {
            setRotateOpen(false);
            onRotate();
          }}
        />
      ) : null}
    </div>
  );
}
