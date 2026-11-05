import { useId, useState } from 'react';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Field, FieldDescription, FieldLabel } from '@/components/ui/field';
import { Input } from '@/components/ui/input';
import type { Webhook } from '@/lib/api/endpoints';
import { toastManager } from '@/lib/toast';
import { useSetWebhookEnabled } from '@/lib/webhooks/queries';

export interface DisableWebhookDialogProps {
  appId: string;
  /** The webhook to disable; `undefined` closes the dialog. */
  webhook: Webhook | undefined;
  onOpenChange: (open: boolean) => void;
}

/**
 * C.13 disable confirmation with an optional reason. The reason is shown on
 * the list and the detail header; enabling again clears it.
 */
export function DisableWebhookDialog({
  appId,
  webhook,
  onOpenChange,
}: DisableWebhookDialogProps) {
  const setEnabled = useSetWebhookEnabled(appId);
  const [reason, setReason] = useState('');
  const reasonId = useId();

  async function disable(): Promise<void> {
    if (webhook === undefined) return;
    try {
      await setEnabled.mutateAsync({
        webhookId: webhook.id,
        enabled: false,
        reason,
      });
      toastManager.add({ type: 'success', title: 'Webhook disabled' });
      setReason('');
      onOpenChange(false);
    } catch {
      // The global mutation toast carries the server message.
    }
  }

  return (
    <Dialog
      open={webhook !== undefined}
      onOpenChange={(open, details) => {
        if (!open && details.reason === 'focus-out') return;
        if (!open) setReason('');
        onOpenChange(open);
      }}
    >
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Disable webhook</DialogTitle>
          <DialogDescription>
            Disabled webhooks don&apos;t deliver events. You can enable it again
            at any time.
          </DialogDescription>
        </DialogHeader>
        <Field>
          <FieldLabel htmlFor={reasonId}>Reason (optional)</FieldLabel>
          <Input
            id={reasonId}
            value={reason}
            placeholder="For example: downstream maintenance"
            onChange={(event) => {
              setReason(event.target.value);
            }}
          />
          <FieldDescription>
            Shown next to the status so the pause is explained later.
          </FieldDescription>
        </Field>
        <DialogFooter>
          <Button
            type="button"
            variant="secondary"
            onClick={() => {
              onOpenChange(false);
            }}
          >
            Cancel
          </Button>
          <Button
            type="button"
            variant="destructive"
            disabled={setEnabled.isPending}
            onClick={() => {
              void disable();
            }}
          >
            {setEnabled.isPending ? 'Disabling…' : 'Disable webhook'}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
