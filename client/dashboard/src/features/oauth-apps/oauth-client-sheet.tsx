import { Loader2, Plus } from 'lucide-react';
import { useEffect, useId, useRef, useState } from 'react';
import { IdChip } from '@/components/data/id-chip';
import { RelativeTime } from '@/components/data/relative-time';
import { ConfirmDialog } from '@/components/feedback/confirm-dialog';
import { Button } from '@/components/ui/button';
import { Field, FieldError, FieldLabel } from '@/components/ui/field';
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
import { accountTimestamp } from '@/lib/account/dates';
import type {
  OauthApp,
  OauthAppClient,
  OauthAppClientSecret,
} from '@/lib/api/endpoints';
import { formatCount } from '@/lib/format';
import { oauthAppErrorMessage } from '@/lib/oauth-apps/errors';
import {
  cleanRedirectUrls,
  clientNameProblem,
  redirectUrlsProblem,
} from '@/lib/oauth-apps/model';
import {
  useCreateClientSecret,
  useCreateOauthClient,
  useDeleteClientSecret,
  useUpdateOauthClient,
} from '@/lib/oauth-apps/queries';
import { toastManager } from '@/lib/toast';

const CREATE_ERROR = "Couldn't create the client. Try again.";
const SAVE_ERROR = "Couldn't save the client. Try again.";
const SECRET_ERROR = "Couldn't create a client secret. Try again.";
const DELETE_SECRET_ERROR = "Couldn't delete the client secret. Try again.";

export interface OauthClientSheetProps {
  appId: string;
  oauthApp: OauthApp;
  /** The client to edit, or undefined to create one. */
  client?: OauthAppClient;
  onClose: () => void;
  /** Opens the one-time secret dialog; the sheet stays open underneath. */
  onSecret: (secret: { clientName: string; value: string }) => void;
}

/**
 * C.14 client sheet: name and redirect URL rows, plus the client's secrets.
 * Redirect URLs are edited as a list, but the server applies one add or remove
 * at a time, so the mutation sends the diff.
 */
export function OauthClientSheet({
  appId,
  oauthApp,
  client,
  onClose,
  onSecret,
}: OauthClientSheetProps) {
  const create = useCreateOauthClient(appId);
  const update = useUpdateOauthClient(appId);
  const createSecret = useCreateClientSecret(appId);
  const deleteSecret = useDeleteClientSecret(appId);

  const [name, setName] = useState(client?.clientName ?? '');
  const [urls, setUrls] = useState<string[]>(
    client === undefined ? [''] : [...client.authorizedRedirectUrls],
  );
  const [attempted, setAttempted] = useState(false);
  const [discardOpen, setDiscardOpen] = useState(false);
  const [secretTarget, setSecretTarget] = useState<OauthAppClientSecret>();

  const nameId = useId();
  const redirectLabelId = useId();
  const redirectErrorId = `${redirectLabelId}-error`;
  const nameRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    nameRef.current?.focus();
  }, []);

  const testMode = !oauthApp.isPublic;
  const nextUrls = cleanRedirectUrls(urls);
  const dirty =
    client === undefined
      ? name !== '' || nextUrls.length > 0
      : name !== client.clientName ||
        nextUrls.join('\n') !== client.authorizedRedirectUrls.join('\n');

  const nameProblem = attempted ? clientNameProblem(name) : undefined;
  const redirectProblem = attempted
    ? redirectUrlsProblem(urls, testMode)
    : undefined;
  const pending = create.isPending || update.isPending;
  const mutationError = create.isError
    ? create.error
    : update.isError
      ? update.error
      : undefined;

  function requestClose(): void {
    if (pending) return;
    if (dirty) {
      setDiscardOpen(true);
      return;
    }
    onClose();
  }

  function setUrlAt(index: number, value: string): void {
    setUrls((current) =>
      current.map((url, position) => (position === index ? value : url)),
    );
    create.reset();
    update.reset();
  }

  function submit(): void {
    if (pending) return;
    setAttempted(true);
    if (clientNameProblem(name) !== undefined) return;
    if (redirectUrlsProblem(urls, testMode) !== undefined) return;

    if (client === undefined) {
      create.mutate(
        {
          oauthAppId: oauthApp.id,
          name: name.trim(),
          redirectUrls: nextUrls,
        },
        {
          onSuccess: (result) => {
            toastManager.add({
              type: 'success',
              title: 'OAuth client created',
            });
            onSecret({
              clientName: result.client.clientName,
              value: result.secretValue,
            });
            onClose();
          },
        },
      );
      return;
    }

    update.mutate(
      {
        oauthAppId: oauthApp.id,
        client,
        name: name.trim(),
        redirectUrls: nextUrls,
      },
      {
        onSuccess: () => {
          toastManager.add({ type: 'success', title: 'OAuth client saved' });
          onClose();
        },
      },
    );
  }

  const secrets = client?.clientSecrets ?? [];

  return (
    <>
      <Sheet
        open
        onOpenChange={(open) => {
          if (!open) requestClose();
        }}
      >
        <SheetContent size="md">
          <SheetHeader>
            <SheetTitle>
              {client === undefined ? 'New client' : 'Edit client'}
            </SheetTitle>
            <SheetDescription>
              A client is one integration that signs users in to this app.
            </SheetDescription>
          </SheetHeader>

          <form
            className="flex min-h-0 flex-1 flex-col"
            onSubmit={(event) => {
              event.preventDefault();
              submit();
            }}
          >
            <SheetBody className="flex flex-col gap-4">
              <Field invalid={nameProblem !== undefined}>
                <FieldLabel htmlFor={nameId}>Name</FieldLabel>
                <Input
                  id={nameId}
                  ref={nameRef}
                  autoComplete="off"
                  value={name}
                  disabled={pending}
                  aria-invalid={nameProblem !== undefined}
                  aria-describedby={
                    nameProblem === undefined ? undefined : `${nameId}-error`
                  }
                  onChange={(event) => {
                    setName(event.target.value);
                    create.reset();
                    update.reset();
                  }}
                />
                {nameProblem === undefined ? null : (
                  <FieldError id={`${nameId}-error`} role="alert">
                    {nameProblem}
                  </FieldError>
                )}
              </Field>

              {client === undefined ? null : (
                <div className="flex flex-col gap-1">
                  <span className="text-sm font-medium">Client ID</span>
                  <IdChip value={client.clientId} head={8} tail={4} />
                </div>
              )}

              <div
                role="group"
                aria-labelledby={redirectLabelId}
                aria-describedby={
                  redirectProblem === undefined ? undefined : redirectErrorId
                }
                className="flex flex-col gap-2"
              >
                <span id={redirectLabelId} className="text-sm font-medium">
                  Redirect URLs
                </span>
                <p className="text-muted-foreground text-xs">
                  Where the client may send users after they sign in.
                </p>
                {urls.length === 0 ? (
                  <p className="text-muted-foreground text-xs">
                    No redirect URLs
                  </p>
                ) : (
                  <ul className="flex flex-col gap-2">
                    {urls.map((url, index) => (
                      <li key={index} className="flex items-center gap-2">
                        <Input
                          aria-label={`Redirect URL ${formatCount(index + 1)}`}
                          inputMode="url"
                          placeholder="https://example.com/callback"
                          value={url}
                          disabled={pending}
                          aria-invalid={redirectProblem !== undefined}
                          aria-describedby={
                            redirectProblem === undefined
                              ? undefined
                              : redirectErrorId
                          }
                          onChange={(event) => {
                            setUrlAt(index, event.target.value);
                          }}
                        />
                        <Button
                          type="button"
                          variant="ghost"
                          size="sm"
                          disabled={pending}
                          aria-label={`Remove redirect URL ${formatCount(index + 1)}`}
                          onClick={() => {
                            setUrls((current) =>
                              current.filter(
                                (_value, position) => position !== index,
                              ),
                            );
                            create.reset();
                            update.reset();
                          }}
                        >
                          Remove
                        </Button>
                      </li>
                    ))}
                  </ul>
                )}
                {redirectProblem === undefined ? null : (
                  <p
                    id={redirectErrorId}
                    role="alert"
                    className="text-destructive text-xs"
                  >
                    {redirectProblem}
                  </p>
                )}
                <div>
                  <Button
                    type="button"
                    variant="secondary"
                    size="sm"
                    disabled={pending}
                    onClick={() => {
                      setUrls((current) => [...current, '']);
                    }}
                  >
                    <Plus aria-hidden="true" />
                    Add redirect URL
                  </Button>
                </div>
                {testMode ? (
                  <p className="text-muted-foreground text-xs">
                    Test-mode apps may use localhost over http. Public apps need
                    https.
                  </p>
                ) : null}
              </div>

              {client === undefined ? null : (
                <div className="border-border flex flex-col gap-2 rounded-md border p-3">
                  <div className="flex flex-wrap items-center justify-between gap-2">
                    <span className="text-sm font-medium">Client secrets</span>
                    <Button
                      type="button"
                      variant="secondary"
                      size="sm"
                      disabled={createSecret.isPending}
                      aria-busy={createSecret.isPending}
                      onClick={() => {
                        createSecret.reset();
                        createSecret.mutate(
                          {
                            oauthAppId: oauthApp.id,
                            clientId: client.clientId,
                          },
                          {
                            onSuccess: (result) => {
                              onSecret({
                                clientName: client.clientName,
                                value: result.secretValue,
                              });
                            },
                          },
                        );
                      }}
                    >
                      {createSecret.isPending ? (
                        <Loader2 className="animate-spin" aria-hidden="true" />
                      ) : (
                        <Plus aria-hidden="true" />
                      )}
                      Create secret
                    </Button>
                  </div>
                  {secrets.length === 0 ? (
                    <p className="text-muted-foreground text-xs">
                      No client secrets
                    </p>
                  ) : (
                    <ul className="flex flex-col gap-1">
                      {secrets.map((secret) => (
                        <li
                          key={secret.id}
                          className="flex flex-wrap items-center gap-x-3 gap-y-1"
                        >
                          <code className="text-cell font-mono">
                            {secret.firstFour}••••••••
                          </code>
                          <RelativeTime
                            value={accountTimestamp(secret.createdAt)}
                            className="text-muted-foreground text-xs"
                          />
                          <Button
                            type="button"
                            variant="ghost"
                            size="sm"
                            className="ml-auto"
                            disabled={deleteSecret.isPending}
                            onClick={() => {
                              deleteSecret.reset();
                              setSecretTarget(secret);
                            }}
                          >
                            Delete
                          </Button>
                        </li>
                      ))}
                    </ul>
                  )}
                  {createSecret.isError ? (
                    <p role="alert" className="text-destructive text-xs">
                      {oauthAppErrorMessage(createSecret.error, SECRET_ERROR)}
                    </p>
                  ) : null}
                  {deleteSecret.isError ? (
                    <p role="alert" className="text-destructive text-xs">
                      {oauthAppErrorMessage(
                        deleteSecret.error,
                        DELETE_SECRET_ERROR,
                      )}
                    </p>
                  ) : null}
                </div>
              )}

              {mutationError === undefined ? null : (
                <p role="alert" className="text-destructive text-sm">
                  {oauthAppErrorMessage(
                    mutationError,
                    client === undefined ? CREATE_ERROR : SAVE_ERROR,
                  )}
                </p>
              )}
            </SheetBody>

            <SheetFooter>
              <Button
                type="button"
                variant="secondary"
                disabled={pending}
                onClick={requestClose}
              >
                Cancel
              </Button>
              <Button
                type="submit"
                disabled={
                  pending ||
                  name.trim() === '' ||
                  (client !== undefined && !dirty)
                }
                aria-busy={pending}
              >
                {pending ? (
                  <Loader2 className="animate-spin" aria-hidden="true" />
                ) : null}
                {client === undefined ? 'Create client' : 'Save'}
              </Button>
            </SheetFooter>
          </form>
        </SheetContent>
      </Sheet>

      <ConfirmDialog
        open={discardOpen}
        onOpenChange={setDiscardOpen}
        title="Discard unsaved changes?"
        description="The client has not been saved yet."
        confirmLabel="Discard"
        cancelLabel="Keep editing"
        onConfirm={() => {
          setDiscardOpen(false);
          onClose();
        }}
      />

      <ConfirmDialog
        open={secretTarget !== undefined}
        onOpenChange={(open) => {
          if (open) return;
          setSecretTarget(undefined);
          deleteSecret.reset();
        }}
        title="Delete client secret?"
        description={
          <>
            The secret starting with{' '}
            <strong className="font-semibold">{secretTarget?.firstFour}</strong>{' '}
            stops working immediately. Remove it from your code first.
          </>
        }
        confirmLabel="Delete client secret"
        confirming={deleteSecret.isPending}
        onConfirm={() => {
          if (secretTarget === undefined || client === undefined) return;
          deleteSecret.mutate(
            {
              oauthAppId: oauthApp.id,
              clientId: client.clientId,
              clientSecretId: secretTarget.id,
            },
            {
              onSuccess: () => {
                setSecretTarget(undefined);
              },
            },
          );
        }}
      />
    </>
  );
}
