import { useEffect, useRef, useState } from 'react';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Field, FieldLabel } from '@/components/ui/field';
import { Input } from '@/components/ui/input';
import type { AppInstantClient } from '@/lib/instant';
import { showErrorToast, toastManager } from '@/lib/toast';
import { namespaceNameProblem, newNamespaceOps } from '@/lib/explorer/writes';

export interface NewNamespaceDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  client: AppInstantClient;
  /** Namespaces already in the app, for the duplicate check. */
  existing: ReadonlyArray<string>;
  onCreated: (namespace: string) => void;
}

/**
 * C.6 "New namespace". A namespace exists as soon as its `id` attribute does,
 * so creating one pushes that attribute exactly as the old Explorer did; the
 * schema subscription then adds it to the sidebar.
 */
export function NewNamespaceDialog({
  open,
  onOpenChange,
  client,
  existing,
  onCreated,
}: NewNamespaceDialogProps) {
  const [name, setName] = useState('');
  const [error, setError] = useState<string>();
  const [creating, setCreating] = useState(false);
  const nameRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (open) nameRef.current?.focus();
  }, [open]);

  function reset(): void {
    setName('');
    setError(undefined);
    setCreating(false);
  }

  async function create(): Promise<void> {
    if (creating) return;
    const problem = namespaceNameProblem(name, existing);
    if (problem !== undefined) {
      setError(problem);
      return;
    }
    const namespace = name.trim();
    setCreating(true);
    setError(undefined);
    try {
      await client.core._reactor.pushOps(newNamespaceOps(namespace));
      toastManager.add({
        type: 'success',
        title: `Namespace ${namespace} created`,
      });
      onCreated(namespace);
      onOpenChange(false);
      reset();
    } catch (failure) {
      showErrorToast(failure);
      setError('Could not create the namespace. Try again.');
    } finally {
      setCreating(false);
    }
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next) reset();
        onOpenChange(next);
      }}
    >
      <DialogContent>
        <DialogHeader>
          <DialogTitle>New namespace</DialogTitle>
          <DialogDescription>
            A namespace starts empty. Add its `id` attribute now; the rest
            appears on the first write.
          </DialogDescription>
        </DialogHeader>

        <div className="mt-4 flex flex-col gap-3">
          <Field>
            <FieldLabel>Namespace name</FieldLabel>
            <Input
              value={name}
              ref={nameRef}
              autoComplete="off"
              spellCheck={false}
              placeholder="todos"
              aria-label="Namespace name"
              aria-invalid={error === undefined ? undefined : true}
              onChange={(event) => {
                setName(event.target.value);
                setError(undefined);
              }}
              onKeyDown={(event) => {
                if (event.key !== 'Enter') return;
                event.preventDefault();
                void create();
              }}
            />
            {error === undefined ? null : (
              <p role="alert" className="text-destructive text-xs font-medium">
                {error}
              </p>
            )}
          </Field>
        </div>

        <DialogFooter>
          <Button
            type="button"
            variant="secondary"
            disabled={creating}
            onClick={() => {
              onOpenChange(false);
            }}
          >
            Cancel
          </Button>
          <Button
            type="button"
            disabled={creating || name.trim() === ''}
            onClick={() => {
              void create();
            }}
          >
            {creating ? 'Creating…' : 'Create namespace'}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
