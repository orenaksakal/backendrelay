import { X } from 'lucide-react';
import { useId, useState } from 'react';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
import {
  Combobox,
  ComboboxContent,
  ComboboxInput,
  ComboboxInputGroup,
  ComboboxItem,
  ComboboxList,
  ComboboxTrigger,
} from '@/components/ui/combobox';
import {
  Field,
  FieldDescription,
  FieldGroup,
  FieldLabel,
} from '@/components/ui/field';
import { Input } from '@/components/ui/input';
import {
  Sheet,
  SheetBody,
  SheetContent,
  SheetDescription,
  SheetFooter,
  SheetHeader,
  SheetTitle,
} from '@/components/ui/sheet';
import { isApiError } from '@/lib/api/client';
import type { Webhook, WebhookAction } from '@/lib/api/endpoints';
import { toastManager } from '@/lib/toast';
import {
  ACTION_LABELS,
  MAX_ACTIVE_WEBHOOKS,
  WEBHOOK_ACTIONS,
  namespaceOptions,
  normalizeActions,
  sortedNamespaces,
  webhookBody,
  webhookDraft,
  webhookDraftProblem,
  webhookUrlProblem,
  type WebhookDraft,
} from '@/lib/webhooks/model';
import { useCreateWebhook, useUpdateWebhook } from '@/lib/webhooks/queries';

export interface WebhookFormSheetProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  appId: string;
  /** `undefined` creates a webhook; otherwise edits this one. */
  webhook?: Webhook;
  schemaNamespaces: readonly string[];
  canManage: boolean;
  /** A new webhook cannot exceed the server's active-webhook cap. */
  limitReached: boolean;
}

/**
 * C.13 create/edit sheet (672px): an https-only URL, the schema namespaces
 * this webhook watches and the actions it delivers. The namespace picker also
 * keeps names the webhook already references so they can be deselected.
 */
export function WebhookFormSheet({
  open,
  onOpenChange,
  appId,
  webhook,
  schemaNamespaces,
  canManage,
  limitReached,
}: WebhookFormSheetProps) {
  const editing = webhook !== undefined;
  const createWebhook = useCreateWebhook(appId);
  const updateWebhook = useUpdateWebhook(appId);
  const [draft, setDraft] = useState<WebhookDraft>(() => webhookDraft(webhook));
  const [error, setError] = useState<string>();
  const [urlError, setUrlError] = useState<string>();
  const urlId = useId();
  const namespaceOptionsList = namespaceOptions(
    schemaNamespaces,
    draft.namespaces,
  );

  function patch(next: Partial<WebhookDraft>): void {
    setDraft((current) => ({ ...current, ...next }));
    setError(undefined);
  }

  function toggleAction(action: WebhookAction, checked: boolean): void {
    const selected = checked
      ? normalizeActions([...draft.actions, action])
      : draft.actions.filter((entry) => entry !== action);
    patch({ actions: selected });
  }

  async function save(): Promise<void> {
    const currentUrlError = webhookUrlProblem(draft.url);
    setUrlError(currentUrlError);
    const problem = webhookDraftProblem(draft);
    if (problem !== undefined) {
      setError(problem);
      return;
    }
    if (!editing && limitReached) {
      setError(
        `This app already has ${String(MAX_ACTIVE_WEBHOOKS)} active webhooks.`,
      );
      return;
    }
    setError(undefined);
    const body = webhookBody(draft);
    try {
      if (webhook !== undefined) {
        await updateWebhook.mutateAsync({ webhookId: webhook.id, ...body });
        toastManager.add({ type: 'success', title: 'Webhook updated' });
      } else {
        await createWebhook.mutateAsync(body);
        toastManager.add({ type: 'success', title: 'Webhook created' });
      }
      onOpenChange(false);
    } catch (failure) {
      setError(
        isApiError(failure) ? failure.message : "Couldn't save the webhook.",
      );
    }
  }

  const busy = createWebhook.isPending || updateWebhook.isPending;

  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent size="md">
        <SheetHeader>
          <SheetTitle>{editing ? 'Edit webhook' : 'Create webhook'}</SheetTitle>
          <SheetDescription>
            Deliver signed HTTP callbacks when records in the selected
            namespaces change.
          </SheetDescription>
        </SheetHeader>
        <SheetBody>
          <FieldGroup>
            {error === undefined ? null : (
              <p role="alert" className="text-destructive text-sm font-medium">
                {error}
              </p>
            )}

            <Field>
              <FieldLabel htmlFor={urlId}>URL</FieldLabel>
              <Input
                id={urlId}
                type="url"
                inputMode="url"
                autoComplete="off"
                placeholder="https://example.com/webhooks/instant"
                value={draft.url}
                aria-invalid={urlError !== undefined}
                aria-describedby={`${urlId}-description`}
                onChange={(event) => {
                  patch({ url: event.target.value });
                  setUrlError(undefined);
                }}
              />
              {urlError === undefined ? null : (
                <p
                  role="alert"
                  className="text-destructive text-xs font-medium"
                >
                  {urlError}
                </p>
              )}
              <FieldDescription id={`${urlId}-description`}>
                Must be a public https URL. Localhost and private addresses are
                rejected.
              </FieldDescription>
            </Field>

            <Field>
              <FieldLabel>Namespaces</FieldLabel>
              <Combobox
                multiple
                items={namespaceOptionsList}
                value={draft.namespaces}
                onValueChange={(value) => {
                  patch({
                    namespaces: sortedNamespaces(value as readonly string[]),
                  });
                }}
              >
                <ComboboxInputGroup>
                  <ComboboxInput placeholder="Search namespaces" />
                  <ComboboxTrigger aria-label="Show namespaces" />
                </ComboboxInputGroup>
                <ComboboxContent>
                  <ComboboxList>
                    {namespaceOptionsList.map((namespace) => (
                      <ComboboxItem key={namespace} value={namespace}>
                        <span className="font-mono text-xs">{namespace}</span>
                      </ComboboxItem>
                    ))}
                  </ComboboxList>
                </ComboboxContent>
              </Combobox>
              {draft.namespaces.length === 0 ? (
                <FieldDescription>
                  {schemaNamespaces.length === 0
                    ? 'Add namespaces in the schema before creating a webhook.'
                    : 'Pick at least one namespace.'}
                </FieldDescription>
              ) : (
                <div className="flex flex-wrap gap-1">
                  {draft.namespaces.map((namespace) => (
                    <span
                      key={namespace}
                      className="bg-muted text-muted-foreground inline-flex h-6 items-center gap-0.5 rounded-md pr-0.5 pl-2 text-xs"
                    >
                      <span className="font-mono">{namespace}</span>
                      <button
                        type="button"
                        aria-label={`Remove ${namespace}`}
                        className="hover:bg-background inline-flex size-6 items-center justify-center rounded-sm"
                        onClick={() => {
                          patch({
                            namespaces: draft.namespaces.filter(
                              (entry) => entry !== namespace,
                            ),
                          });
                        }}
                      >
                        <X className="size-3" aria-hidden="true" />
                      </button>
                    </span>
                  ))}
                </div>
              )}
            </Field>

            <fieldset className="flex flex-col gap-2">
              <legend className="text-sm font-medium">Actions</legend>
              <div className="flex flex-wrap gap-4">
                {WEBHOOK_ACTIONS.map((action) => (
                  <Field key={action} orientation="horizontal">
                    <Checkbox
                      id={`${urlId}-${action}`}
                      aria-label={ACTION_LABELS[action]}
                      checked={draft.actions.includes(action)}
                      onCheckedChange={(checked) => {
                        toggleAction(action, checked);
                      }}
                    />
                    <FieldLabel htmlFor={`${urlId}-${action}`}>
                      {ACTION_LABELS[action]}
                    </FieldLabel>
                  </Field>
                ))}
              </div>
              <p className="text-muted-foreground text-xs">
                Up to {String(MAX_ACTIVE_WEBHOOKS)} active webhooks per app.
              </p>
            </fieldset>
          </FieldGroup>
        </SheetBody>
        <SheetFooter>
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
            disabled={busy || !canManage}
            onClick={() => {
              void save();
            }}
          >
            {busy ? 'Saving…' : editing ? 'Save webhook' : 'Create webhook'}
          </Button>
        </SheetFooter>
      </SheetContent>
    </Sheet>
  );
}
