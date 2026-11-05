import { Loader2 } from 'lucide-react';
import { useEffect, useId, useRef, useState } from 'react';
import { ConfirmDialog } from '@/components/feedback/confirm-dialog';
import { Button } from '@/components/ui/button';
import {
  Field,
  FieldDescription,
  FieldError,
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
import type { OauthApp } from '@/lib/api/endpoints';
import {
  EMPTY_OAUTH_APP_INFO,
  OAUTH_APP_LOGO_ACCEPT,
  oauthAppInfoEqual,
  oauthAppInfoProblems,
  logoProblem,
  readFileAsDataUrl,
  type OauthAppInfo,
  type OauthAppInfoField,
  type OauthAppInfoProblems,
} from '@/lib/oauth-apps/model';
import { useCreateOauthApp } from '@/lib/oauth-apps/queries';
import { oauthAppErrorMessage } from '@/lib/oauth-apps/errors';
import { toastManager } from '@/lib/toast';

const CREATE_ERROR = "Couldn't create the OAuth app. Try again.";

export interface CreateOauthAppSheetProps {
  appId: string;
  onClose: () => void;
  onCreated: (app: OauthApp) => void;
}

/**
 * C.14 create flow. The sheet keeps the six app-info fields together; the logo
 * is read locally and sent as the data URL the server stores.
 */
export function CreateOauthAppSheet({
  appId,
  onClose,
  onCreated,
}: CreateOauthAppSheetProps) {
  const create = useCreateOauthApp(appId);
  const [info, setInfo] = useState<OauthAppInfo>(EMPTY_OAUTH_APP_INFO);
  const [attempted, setAttempted] = useState(false);
  const [logoError, setLogoError] = useState<string>();
  const [discardOpen, setDiscardOpen] = useState(false);
  const logoInput = useRef<HTMLInputElement>(null);
  const nameRef = useRef<HTMLInputElement>(null);

  const nameId = useId();
  const homePageId = useId();
  const privacyId = useId();
  const termsId = useId();
  const emailId = useId();
  const logoId = useId();

  useEffect(() => {
    nameRef.current?.focus();
  }, []);

  const problems: OauthAppInfoProblems = attempted
    ? oauthAppInfoProblems(info)
    : {};
  const dirty = !oauthAppInfoEqual(info, EMPTY_OAUTH_APP_INFO);

  function field(fieldName: OauthAppInfoField): {
    id: string;
    error: string | undefined;
    errorId: string;
    invalid: boolean;
  } {
    const id = {
      name: nameId,
      homePage: homePageId,
      privacyPolicy: privacyId,
      terms: termsId,
      supportEmail: emailId,
      logo: logoId,
    }[fieldName];
    const error = fieldName === 'logo' ? logoError : problems[fieldName];
    return {
      id,
      error,
      errorId: `${id}-error`,
      invalid: error !== undefined,
    };
  }

  function update(patch: Partial<OauthAppInfo>): void {
    setInfo((current) => ({ ...current, ...patch }));
    create.reset();
  }

  function requestClose(): void {
    if (create.isPending) return;
    if (dirty) {
      setDiscardOpen(true);
      return;
    }
    onClose();
  }

  async function onLogoFile(file: File | undefined): Promise<void> {
    if (file === undefined) return;
    const problem = logoProblem(file);
    if (problem !== undefined) {
      setLogoError(problem);
      return;
    }
    try {
      const dataUrl = await readFileAsDataUrl(file);
      setLogoError(undefined);
      update({ logo: dataUrl });
    } catch {
      setLogoError('Could not read that file. Try another one.');
    }
  }

  function submit(): void {
    if (create.isPending) return;
    setAttempted(true);
    const nextProblems = oauthAppInfoProblems(info);
    if (Object.keys(nextProblems).length > 0) {
      nameRef.current?.focus();
      return;
    }
    create.mutate(
      {
        name: info.name.trim(),
        logo: info.logo === '' ? null : info.logo,
        homePage: info.homePage.trim() === '' ? null : info.homePage.trim(),
        privacyPolicy:
          info.privacyPolicy.trim() === '' ? null : info.privacyPolicy.trim(),
        terms: info.terms.trim() === '' ? null : info.terms.trim(),
        supportEmail:
          info.supportEmail.trim() === '' ? null : info.supportEmail.trim(),
      },
      {
        onSuccess: (result) => {
          toastManager.add({ type: 'success', title: 'OAuth app created' });
          onCreated(result.app);
        },
      },
    );
  }

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
            <SheetTitle>Create OAuth app</SheetTitle>
            <SheetDescription>
              Publish this app as an OAuth provider so other tools can act on a
              user&apos;s behalf.
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
              <Field invalid={field('name').invalid}>
                <FieldLabel htmlFor={nameId}>Name</FieldLabel>
                <Input
                  id={nameId}
                  ref={nameRef}
                  autoComplete="off"
                  value={info.name}
                  disabled={create.isPending}
                  aria-invalid={field('name').invalid}
                  aria-describedby={
                    field('name').error === undefined
                      ? undefined
                      : field('name').errorId
                  }
                  onChange={(event) => {
                    update({ name: event.target.value });
                  }}
                />
                {field('name').error === undefined ? null : (
                  <FieldError id={field('name').errorId} role="alert">
                    {field('name').error}
                  </FieldError>
                )}
              </Field>

              <Field invalid={logoError !== undefined}>
                <FieldLabel htmlFor={logoId}>Logo</FieldLabel>
                <div className="flex items-center gap-3">
                  {info.logo === '' ? (
                    <span
                      aria-hidden="true"
                      className="bg-muted text-muted-foreground flex size-12 items-center justify-center rounded-md text-lg font-semibold"
                    >
                      {info.name.trim().charAt(0).toUpperCase() || 'O'}
                    </span>
                  ) : (
                    <img
                      src={info.logo}
                      alt="Logo preview"
                      className="border-border bg-card size-12 rounded-md border object-contain"
                    />
                  )}
                  <input
                    id={logoId}
                    ref={logoInput}
                    type="file"
                    accept={OAUTH_APP_LOGO_ACCEPT}
                    className="hidden"
                    onChange={(event) => {
                      const file = event.target.files?.[0];
                      event.target.value = '';
                      void onLogoFile(file);
                    }}
                  />
                  <Button
                    type="button"
                    variant="secondary"
                    size="sm"
                    disabled={create.isPending}
                    onClick={() => {
                      logoInput.current?.click();
                    }}
                  >
                    {info.logo === '' ? 'Upload logo' : 'Replace logo'}
                  </Button>
                </div>
                <FieldDescription id={`${logoId}-description`}>
                  PNG or SVG, up to 256 KB.
                </FieldDescription>
                {logoError === undefined ? null : (
                  <FieldError id={`${logoId}-error`} role="alert">
                    {logoError}
                  </FieldError>
                )}
              </Field>

              <Field invalid={field('homePage').invalid}>
                <FieldLabel htmlFor={homePageId}>Homepage URL</FieldLabel>
                <Input
                  id={homePageId}
                  inputMode="url"
                  placeholder="https://example.com"
                  value={info.homePage}
                  disabled={create.isPending}
                  aria-invalid={field('homePage').invalid}
                  aria-describedby={
                    field('homePage').error === undefined
                      ? undefined
                      : field('homePage').errorId
                  }
                  onChange={(event) => {
                    update({ homePage: event.target.value });
                  }}
                />
                {field('homePage').error === undefined ? null : (
                  <FieldError id={field('homePage').errorId} role="alert">
                    {field('homePage').error}
                  </FieldError>
                )}
              </Field>

              <Field invalid={field('privacyPolicy').invalid}>
                <FieldLabel htmlFor={privacyId}>Privacy policy URL</FieldLabel>
                <Input
                  id={privacyId}
                  inputMode="url"
                  placeholder="https://example.com/privacy"
                  value={info.privacyPolicy}
                  disabled={create.isPending}
                  aria-invalid={field('privacyPolicy').invalid}
                  aria-describedby={
                    field('privacyPolicy').error === undefined
                      ? undefined
                      : field('privacyPolicy').errorId
                  }
                  onChange={(event) => {
                    update({ privacyPolicy: event.target.value });
                  }}
                />
                {field('privacyPolicy').error === undefined ? null : (
                  <FieldError id={field('privacyPolicy').errorId} role="alert">
                    {field('privacyPolicy').error}
                  </FieldError>
                )}
              </Field>

              <Field invalid={field('terms').invalid}>
                <FieldLabel htmlFor={termsId}>Terms of service URL</FieldLabel>
                <Input
                  id={termsId}
                  inputMode="url"
                  placeholder="https://example.com/terms"
                  value={info.terms}
                  disabled={create.isPending}
                  aria-invalid={field('terms').invalid}
                  aria-describedby={
                    field('terms').error === undefined
                      ? undefined
                      : field('terms').errorId
                  }
                  onChange={(event) => {
                    update({ terms: event.target.value });
                  }}
                />
                {field('terms').error === undefined ? null : (
                  <FieldError id={field('terms').errorId} role="alert">
                    {field('terms').error}
                  </FieldError>
                )}
              </Field>

              <Field invalid={field('supportEmail').invalid}>
                <FieldLabel htmlFor={emailId}>Support email</FieldLabel>
                <Input
                  id={emailId}
                  type="email"
                  autoComplete="off"
                  placeholder="support@example.com"
                  value={info.supportEmail}
                  disabled={create.isPending}
                  aria-invalid={field('supportEmail').invalid}
                  aria-describedby={
                    field('supportEmail').error === undefined
                      ? undefined
                      : field('supportEmail').errorId
                  }
                  onChange={(event) => {
                    update({ supportEmail: event.target.value });
                  }}
                />
                {field('supportEmail').error === undefined ? null : (
                  <FieldError id={field('supportEmail').errorId} role="alert">
                    {field('supportEmail').error}
                  </FieldError>
                )}
              </Field>

              {create.isError ? (
                <p role="alert" className="text-destructive text-sm">
                  {oauthAppErrorMessage(create.error, CREATE_ERROR)}
                </p>
              ) : null}
            </SheetBody>

            <SheetFooter>
              <Button
                type="button"
                variant="secondary"
                disabled={create.isPending}
                onClick={requestClose}
              >
                Cancel
              </Button>
              <Button
                type="submit"
                disabled={create.isPending || info.name.trim() === ''}
                aria-busy={create.isPending}
              >
                {create.isPending ? (
                  <Loader2 className="animate-spin" aria-hidden="true" />
                ) : null}
                Create OAuth app
              </Button>
            </SheetFooter>
          </form>
        </SheetContent>
      </Sheet>

      <ConfirmDialog
        open={discardOpen}
        onOpenChange={setDiscardOpen}
        title="Discard unsaved changes?"
        description="The new OAuth app has not been created yet."
        confirmLabel="Discard"
        cancelLabel="Keep editing"
        onConfirm={() => {
          setDiscardOpen(false);
          onClose();
        }}
      />
    </>
  );
}
