import { useId, useState, type ReactNode } from 'react';
import { CopyButton } from '@/components/data/copy-button';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Field, FieldDescription } from '@/components/ui/field';
import { Input } from '@/components/ui/input';

export interface TypedConfirmDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: string;
  description?: ReactNode;
  /** What the action does, one line each (B.5.3). */
  consequences: ReadonlyArray<ReactNode>;
  /** Exact, case-sensitive text the user has to type. */
  confirmText: string;
  /** The destructive button repeats the verb, for example "Delete app". */
  confirmLabel: string;
  onConfirm: () => void;
  /** Keeps the button disabled while the action runs. */
  confirming?: boolean;
}

/**
 * Tier 3 confirmation (B.4, B.5.3). The destructive button stays disabled
 * until the input matches the expected text exactly, including case.
 */
export function TypedConfirmDialog({
  open,
  onOpenChange,
  title,
  description,
  consequences,
  confirmText,
  confirmLabel,
  onConfirm,
  confirming = false,
}: TypedConfirmDialogProps) {
  const inputId = useId();
  const [draft, setDraft] = useState('');
  const matches = draft === confirmText;

  function close() {
    setDraft('');
    onOpenChange(false);
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        // Esc, backdrop and Cancel all land here; the typed text does not
        // carry over to the next time the dialog opens.
        if (!next) {
          setDraft('');
        }
        onOpenChange(next);
      }}
    >
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          {description ? (
            <DialogDescription>{description}</DialogDescription>
          ) : null}
        </DialogHeader>

        <div className="mt-4 flex flex-col gap-3">
          <ul className="text-muted-foreground flex list-disc flex-col gap-1 pl-5 text-sm">
            {consequences.map((consequence, index) => (
              <li key={index}>{consequence}</li>
            ))}
          </ul>

          <div className="border-border bg-muted flex items-center justify-between gap-2 rounded-md border px-2.5 py-1.5">
            <code className="text-cell font-mono break-all">{confirmText}</code>
            <CopyButton
              value={confirmText}
              aria-label="Copy confirmation text"
            />
          </div>

          <Field>
            <label htmlFor={inputId} className="text-sm font-medium">
              Type <span className="font-mono">{confirmText}</span> to confirm
            </label>
            <Input
              id={inputId}
              value={draft}
              autoComplete="off"
              spellCheck={false}
              onChange={(event) => {
                setDraft(event.target.value);
              }}
            />
            <FieldDescription>
              The button unlocks when the text matches exactly.
            </FieldDescription>
          </Field>
        </div>

        <DialogFooter>
          <Button
            type="button"
            variant="secondary"
            onClick={() => {
              close();
            }}
          >
            Cancel
          </Button>
          <Button
            type="button"
            variant="destructive"
            disabled={!matches || confirming}
            onClick={() => {
              setDraft('');
              onConfirm();
            }}
          >
            {confirming ? 'Working…' : confirmLabel}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
