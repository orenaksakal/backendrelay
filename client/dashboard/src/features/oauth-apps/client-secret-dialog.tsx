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

export interface ClientSecretDialogProps {
  /** The client the secret belongs to, for the title. */
  clientName: string;
  /** The plaintext secret; it is shown once and never fetched again. */
  secret: string;
  onClose: () => void;
}

/**
 * C.14 one-time secret dialog: copy and "I've saved it". There is no close
 * button because the value cannot be recovered after this point.
 */
export function ClientSecretDialog({
  clientName,
  secret,
  onClose,
}: ClientSecretDialogProps) {
  return (
    <Dialog
      open
      onOpenChange={() => {
        // The secret is shown once: only "I've saved it" closes the dialog.
      }}
    >
      <DialogContent showCloseButton={false}>
        <DialogHeader>
          <DialogTitle>Copy the client secret</DialogTitle>
          <DialogDescription>
            This is the only time the secret for {clientName} is shown. Store it
            now; you cannot see it again.
          </DialogDescription>
        </DialogHeader>
        <div className="border-input bg-muted mt-4 flex items-start gap-2 rounded-md border p-3">
          <code className="text-cell min-w-0 flex-1 font-mono break-all">
            {secret}
          </code>
          <CopyButton value={secret} aria-label="Copy client secret" />
        </div>
        <DialogFooter>
          <Button type="button" onClick={onClose}>
            I&apos;ve saved it
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
