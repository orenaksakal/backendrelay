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
import {
  Field,
  FieldDescription,
  FieldError,
  FieldLabel,
} from '@/components/ui/field';
import { Input } from '@/components/ui/input';
import { isApiError } from '@/lib/api/client';
import { createUser, type AppAdminConfig } from '@/lib/users/admin';
import { showErrorToast, toastManager } from '@/lib/toast';

export interface CreateUserDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** `null` disables the form: the app payload has no admin token. */
  config: AppAdminConfig | null;
  onCreated: () => void;
}

/**
 * C.8 "Create user": email only. The call creates the user and returns a
 * refresh token, which is discarded — the screen confirms "User created" and
 * never reveals a credential.
 */
export function CreateUserDialog({
  open,
  onOpenChange,
  config,
  onCreated,
}: CreateUserDialogProps) {
  const emailId = useId();
  const errorId = useId();
  const [email, setEmail] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [pending, setPending] = useState(false);

  function close(): void {
    setEmail('');
    setError(null);
    onOpenChange(false);
  }

  async function submit(): Promise<void> {
    if (pending) return;
    const trimmed = email.trim();
    if (trimmed === '') {
      setError('Enter an email address.');
      return;
    }
    if (config === null) {
      setError(
        "The admin token for this app isn't available. Reload the app to fetch it again.",
      );
      return;
    }

    setPending(true);
    try {
      await createUser(config, trimmed);
      toastManager.add({ type: 'success', title: 'User created' });
      setEmail('');
      setError(null);
      onCreated();
      onOpenChange(false);
    } catch (failure) {
      setError(
        isApiError(failure) && failure.message !== ''
          ? failure.message
          : "Couldn't create the user. Try again.",
      );
      showErrorToast(failure);
    } finally {
      setPending(false);
    }
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next) {
          setEmail('');
          setError(null);
        }
        onOpenChange(next);
      }}
    >
      <DialogContent>
        <form
          className="flex flex-col gap-4"
          onSubmit={(event) => {
            event.preventDefault();
            void submit();
          }}
        >
          <DialogHeader>
            <DialogTitle>Create user</DialogTitle>
            <DialogDescription>
              Adding an email creates a user who can sign in with a magic code.
            </DialogDescription>
          </DialogHeader>

          <Field invalid={error !== null}>
            <FieldLabel htmlFor={emailId}>Email</FieldLabel>
            <Input
              id={emailId}
              type="email"
              autoComplete="email"
              placeholder="user@example.com"
              value={email}
              aria-invalid={error !== null}
              aria-describedby={error === null ? undefined : errorId}
              onChange={(event) => {
                setEmail(event.target.value);
                setError(null);
              }}
            />
            {error === null ? (
              <FieldDescription>
                The user is created even if they never sign in.
              </FieldDescription>
            ) : (
              <FieldError id={errorId} match role="alert">
                {error}
              </FieldError>
            )}
          </Field>

          <DialogFooter>
            <Button
              type="button"
              variant="secondary"
              disabled={pending}
              onClick={close}
            >
              Cancel
            </Button>
            <Button type="submit" disabled={pending} aria-busy={pending}>
              {pending ? 'Creating…' : 'Create user'}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
