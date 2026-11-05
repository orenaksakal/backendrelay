import { Globe, Plus, Smartphone, Square, Triangle } from 'lucide-react';
import { useState } from 'react';
import { RelativeTime } from '@/components/data/relative-time';
import { SimpleTable } from '@/components/data/simple-table';
import { ConfirmDialog } from '@/components/feedback/confirm-dialog';
import { EmptyState } from '@/components/feedback/empty-state';
import { SectionCard } from '@/components/layout/section-card';
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
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import type { AuthorizedOrigin } from '@/lib/api/endpoints';
import {
  ORIGIN_SERVICES,
  ORIGIN_SERVICE_LABELS,
  isOriginService,
  originDisplay,
  originHelp,
  originInputLabel,
  originInputPlaceholder,
  originTypeLabel,
  validateOrigin,
  type OriginService,
} from '@/lib/app-auth/origins';
import { useAddOrigin, useDeleteOrigin } from '@/lib/app-auth/queries';
import { toastManager } from '@/lib/toast';

export interface OriginsTabProps {
  appId: string;
  origins: ReadonlyArray<AuthorizedOrigin>;
  canManage: boolean;
}

function originIcon(service: string) {
  switch (service) {
    case 'netlify':
      return Triangle;
    case 'vercel':
      return Square;
    case 'custom-scheme':
      return Smartphone;
    default:
      return Globe;
  }
}

interface AddOriginDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  appId: string;
}

function AddOriginDialog({ open, onOpenChange, appId }: AddOriginDialogProps) {
  const addOrigin = useAddOrigin(appId);
  const [service, setService] = useState<OriginService>('generic');
  const [value, setValue] = useState('');
  const [error, setError] = useState<string>();

  function reset(): void {
    setService('generic');
    setValue('');
    setError(undefined);
  }

  async function save(): Promise<void> {
    const validated = validateOrigin(service, value);
    if (!validated.ok) {
      setError(validated.message);
      return;
    }
    setError(undefined);
    try {
      await addOrigin.mutateAsync({
        service,
        params: validated.params,
      });
      toastManager.add({ type: 'success', title: 'Redirect origin added' });
      reset();
      onOpenChange(false);
    } catch (failure) {
      setError(
        failure instanceof Error
          ? failure.message
          : "Couldn't add the redirect origin.",
      );
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
          <DialogTitle>Add origin</DialogTitle>
          <DialogDescription>
            Allow OAuth sign-in to start from this site or app.
          </DialogDescription>
        </DialogHeader>
        <div className="mt-4 flex flex-col gap-4">
          <Field>
            <FieldLabel>Type</FieldLabel>
            <Select
              value={service}
              onValueChange={(next) => {
                if (isOriginService(next)) {
                  setService(next);
                  setError(undefined);
                }
              }}
            >
              <SelectTrigger aria-label="Type">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {ORIGIN_SERVICES.map((option) => (
                  <SelectItem key={option} value={option}>
                    {ORIGIN_SERVICE_LABELS[option]}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </Field>
          <Field>
            <FieldLabel htmlFor="origin-value">
              {originInputLabel(service)}
            </FieldLabel>
            <Input
              id="origin-value"
              value={value}
              placeholder={originInputPlaceholder(service)}
              aria-invalid={error !== undefined}
              onChange={(event) => {
                setValue(event.target.value);
                setError(undefined);
              }}
            />
            {originHelp(service) === undefined ? null : (
              <FieldDescription>{originHelp(service)}</FieldDescription>
            )}
          </Field>
          {error ? (
            <p role="alert" className="text-destructive text-sm font-medium">
              {error}
            </p>
          ) : null}
        </div>
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
            disabled={addOrigin.isPending}
            onClick={() => {
              void save();
            }}
          >
            {addOrigin.isPending ? 'Adding…' : 'Add origin'}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/** C.11 redirect origins tab: the table plus the add dialog and Tier 2 delete. */
export function OriginsTab({ appId, origins, canManage }: OriginsTabProps) {
  const deleteOrigin = useDeleteOrigin(appId);
  const [addOpen, setAddOpen] = useState(false);
  const [deleting, setDeleting] = useState<AuthorizedOrigin>();

  async function confirmDelete(): Promise<void> {
    if (deleting === undefined) return;
    try {
      await deleteOrigin.mutateAsync(deleting.id);
      toastManager.add({ type: 'success', title: 'Redirect origin deleted' });
      setDeleting(undefined);
    } catch {
      // The global mutation toast carries the server message.
    }
  }

  return (
    <SectionCard
      title="Redirect origins"
      description="Sites and apps that may start the OAuth flow from their own URL."
      actions={
        <Button
          type="button"
          size="sm"
          disabled={!canManage}
          title={canManage ? undefined : 'This needs the collaborator role.'}
          onClick={() => {
            setAddOpen(true);
          }}
        >
          <Plus aria-hidden="true" />
          Add origin
        </Button>
      }
    >
      <SimpleTable
        caption="Redirect origins"
        columns={[
          {
            id: 'type',
            header: 'Type',
            cell: (origin) => {
              const Icon = originIcon(origin.service);
              return (
                <span className="flex items-center gap-2">
                  <Icon
                    aria-hidden="true"
                    className="text-muted-foreground size-4"
                  />
                  {originTypeLabel(origin.service)}
                </span>
              );
            },
          },
          {
            id: 'value',
            header: 'Value',
            cell: (origin) => (
              <span className="text-cell font-mono">
                {originDisplay(origin)}
              </span>
            ),
          },
          {
            id: 'created',
            header: 'Created',
            cell: (origin) =>
              origin.created_at === undefined ? (
                <span className="text-muted-foreground">—</span>
              ) : (
                <RelativeTime value={origin.created_at} />
              ),
          },
          {
            id: 'actions',
            header: <span className="sr-only">Actions</span>,
            align: 'right',
            cell: (origin) => (
              <Button
                type="button"
                variant="ghost"
                size="sm"
                className="text-destructive"
                disabled={!canManage}
                aria-label={`Delete origin ${originDisplay(origin)}`}
                onClick={() => {
                  setDeleting(origin);
                }}
              >
                Delete
              </Button>
            ),
          },
        ]}
        rows={origins}
        getRowId={(origin) => origin.id}
        empty={
          <EmptyState
            icon={Globe}
            title="No redirect origins"
            description="Add the site or app URL that starts OAuth sign-in."
            action={
              <Button
                type="button"
                size="sm"
                disabled={!canManage}
                title={
                  canManage ? undefined : 'This needs the collaborator role.'
                }
                onClick={() => {
                  setAddOpen(true);
                }}
              >
                Add origin
              </Button>
            }
          />
        }
      />
      <AddOriginDialog open={addOpen} onOpenChange={setAddOpen} appId={appId} />
      <ConfirmDialog
        open={deleting !== undefined}
        onOpenChange={(next) => {
          if (!next) setDeleting(undefined);
        }}
        title="Delete redirect origin"
        description={
          deleting === undefined
            ? ''
            : `Users will no longer be able to start OAuth sign-in from ${originDisplay(deleting)}.`
        }
        confirmLabel="Delete origin"
        confirming={deleteOrigin.isPending}
        onConfirm={() => {
          void confirmDelete();
        }}
      />
    </SectionCard>
  );
}
