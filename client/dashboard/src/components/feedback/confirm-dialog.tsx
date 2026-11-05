import type { ReactNode } from 'react';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';

export interface ConfirmDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: string;
  description: ReactNode;
  /** Repeats the verb, for example "Delete webhook" (B.4). */
  confirmLabel: string;
  /** Defaults to "Cancel"; the unsaved-changes guard says "Keep editing". */
  cancelLabel?: string;
  onConfirm: () => void;
  confirming?: boolean;
}

/**
 * Tier 2 confirmation (B.5.3): destructive but limited. Names the object and
 * the consequence; the destructive button repeats the verb.
 */
export function ConfirmDialog({
  open,
  onOpenChange,
  title,
  description,
  confirmLabel,
  cancelLabel = 'Cancel',
  onConfirm,
  confirming = false,
}: ConfirmDialogProps) {
  return (
    <Dialog
      open={open}
      onOpenChange={(next, details) => {
        // A menu-triggered dialog can receive focus-out while the menu returns
        // focus to its trigger; that should not dismiss the dialog.
        if (!next && details.reason === 'focus-out') return;
        onOpenChange(next);
      }}
    >
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription>{description}</DialogDescription>
        </DialogHeader>
        <DialogFooter>
          <Button
            type="button"
            variant="secondary"
            onClick={() => {
              onOpenChange(false);
            }}
          >
            {cancelLabel}
          </Button>
          <Button
            type="button"
            variant="destructive"
            disabled={confirming}
            onClick={onConfirm}
          >
            {confirming ? 'Working…' : confirmLabel}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
