import { useMutation } from '@tanstack/react-query';
import { Loader2 } from 'lucide-react';
import { useEffect, useId, useRef, useState } from 'react';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Field, FieldError, FieldLabel } from '@/components/ui/field';
import { Input } from '@/components/ui/input';
import { apiFetch } from '@/lib/api/client';
import { orgEndpoints } from '@/lib/api/endpoints';
import { creationErrorMessage, orgNameProblem } from '@/lib/creation/creation';
import { toastManager } from '@/lib/toast';
import { workspaceQueryKeys } from '@/lib/workspace/queries';

const CREATE_ORG_ERROR = "Couldn't create the org. Try again.";

export interface CreateOrgDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

/**
 * Tier-2 create dialog from C.3. The org list refreshes through the bootstrap
 * query, so the new workspace is offered by the app form next time it opens.
 */
export function CreateOrgDialog({ open, onOpenChange }: CreateOrgDialogProps) {
  const [name, setName] = useState('');
  const [nameError, setNameError] = useState<string>();
  const nameId = useId();
  const nameErrorId = `${nameId}-error`;
  const nameRef = useRef<HTMLInputElement>(null);

  const createOrg = useMutation({
    mutationFn: (title: string) =>
      apiFetch(orgEndpoints.create, { body: { title } }),
    meta: { invalidates: [workspaceQueryKeys.bootstrap] },
  });

  useEffect(() => {
    if (open) nameRef.current?.focus();
  }, [open]);

  function close(nextOpen: boolean): void {
    onOpenChange(nextOpen);
    if (nextOpen) return;
    setName('');
    setNameError(undefined);
    createOrg.reset();
  }

  function submit(): void {
    if (createOrg.isPending) return;
    const problem = orgNameProblem(name);
    if (problem !== undefined) {
      setNameError(problem);
      return;
    }
    setNameError(undefined);
    createOrg.mutate(name.trim(), {
      onSuccess: () => {
        toastManager.add({ type: 'success', title: 'Org created' });
        close(false);
      },
    });
  }

  return (
    <Dialog open={open} onOpenChange={close}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Create an org</DialogTitle>
          <DialogDescription>
            An org groups apps and shared members. You become its owner.
          </DialogDescription>
        </DialogHeader>
        <form
          className="mt-4 flex flex-col gap-4"
          onSubmit={(event) => {
            event.preventDefault();
            submit();
          }}
        >
          <Field invalid={nameError !== undefined}>
            <FieldLabel htmlFor={nameId}>Org name</FieldLabel>
            <Input
              id={nameId}
              ref={nameRef}
              value={name}
              disabled={createOrg.isPending}
              aria-invalid={nameError !== undefined}
              aria-describedby={
                nameError === undefined ? undefined : nameErrorId
              }
              onChange={(event) => {
                setName(event.target.value);
                setNameError(undefined);
                createOrg.reset();
              }}
            />
            {nameError === undefined ? null : (
              <FieldError id={nameErrorId} match role="alert">
                {nameError}
              </FieldError>
            )}
          </Field>

          {createOrg.isError ? (
            <p role="alert" className="text-destructive text-sm">
              {creationErrorMessage(createOrg.error, CREATE_ORG_ERROR)}
            </p>
          ) : null}

          <DialogFooter className="mt-0">
            <Button
              type="button"
              variant="secondary"
              disabled={createOrg.isPending}
              onClick={() => {
                close(false);
              }}
            >
              Cancel
            </Button>
            <Button
              type="submit"
              disabled={createOrg.isPending || name.trim() === ''}
              aria-busy={createOrg.isPending}
            >
              {createOrg.isPending ? (
                <Loader2 className="animate-spin" aria-hidden="true" />
              ) : null}
              Create org
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
