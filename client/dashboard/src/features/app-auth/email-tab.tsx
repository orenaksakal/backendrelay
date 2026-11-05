import { RotateCcw, Send, ShieldCheck } from 'lucide-react';
import { useId, useRef, useState } from 'react';
import {
  CodeEditor,
  type CodeEditorHandle,
} from '@/components/code-editor/code-editor';
import { ConfirmDialog } from '@/components/feedback/confirm-dialog';
import { ErrorState } from '@/components/feedback/error-state';
import { CardSkeleton } from '@/components/feedback/skeletons';
import { SectionCard } from '@/components/layout/section-card';
import { SettingsRow } from '@/components/layout/settings-row';
import { UnsavedChangesBar } from '@/components/layout/unsaved-changes-bar';
import { Badge } from '@/components/ui/badge';
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
import { isApiError } from '@/lib/api/client';
import type { SenderVerification } from '@/lib/api/endpoints';
import type { EffectiveRole } from '@/lib/api/endpoints/schemas';
import {
  EMAIL_TEMPLATE_VARIABLES,
  MAGIC_CODE_EXPIRY_MAX,
  MAGIC_CODE_EXPIRY_MIN,
  effectiveExpiryMinutes,
  effectiveSenderName,
  emailTemplateDirty,
  emailTemplateDraft,
  emailTemplateProblem,
  expiryMinutesProblem,
  senderStatus,
  substituteSampleVars,
  type EmailTemplateDraft,
  type EmailTemplateVariable,
} from '@/lib/app-auth/email';
import {
  useDeleteEmailTemplate,
  useSaveEmailTemplate,
  useSendSenderVerificationCode,
  useSendTestEmail,
  useSetMagicCodeExpiry,
  useVerifySenderCode,
} from '@/lib/app-auth/queries';
import { useSenderVerification } from '@/lib/apps/overview-queries';
import { useAppDetails } from '@/lib/apps/settings-queries';
import { useSession } from '@/lib/auth/session';
import { roleRank } from '@/lib/organization/model';
import { toastManager } from '@/lib/toast';

const EMAIL_PATTERN = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;

export interface EmailTabProps {
  appId: string;
  appTitle: string;
  role: EffectiveRole;
  defaultSenderEmail: string;
}

interface SenderDialogProps {
  appTitle: string;
  senderEmail: string;
  senderName: string;
  onApply: (senderEmail: string, senderName: string) => void;
  onOpenChange: (open: boolean) => void;
}

function SenderDialog({
  appTitle,
  senderEmail,
  senderName,
  onApply,
  onOpenChange,
}: SenderDialogProps) {
  const [email, setEmail] = useState(senderEmail);
  const [name, setName] = useState(effectiveSenderName(senderName, appTitle));
  const [error, setError] = useState<string>();

  function submit(): void {
    const trimmed = email.trim().toLowerCase();
    if (!EMAIL_PATTERN.test(trimmed)) {
      setError('Enter a valid email address.');
      return;
    }
    onApply(trimmed, name.trim());
    onOpenChange(false);
  }

  return (
    <Dialog open onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Use your own domain</DialogTitle>
          <DialogDescription>
            The address must be on a domain with Email Sending enabled. Saving
            the email settings sends a verification code to it.
          </DialogDescription>
        </DialogHeader>
        <div className="mt-4 flex flex-col gap-4">
          <Field>
            <FieldLabel htmlFor="sender-email">Sender email</FieldLabel>
            <Input
              id="sender-email"
              type="email"
              autoComplete="off"
              value={email}
              placeholder="you@yourdomain.com"
              aria-invalid={error !== undefined}
              onChange={(event) => {
                setEmail(event.target.value);
                setError(undefined);
              }}
            />
            {error === undefined ? null : (
              <FieldDescription role="alert" className="text-destructive">
                {error}
              </FieldDescription>
            )}
          </Field>
          <Field>
            <FieldLabel htmlFor="sender-name">Sender name</FieldLabel>
            <Input
              id="sender-name"
              autoComplete="off"
              value={name}
              placeholder={appTitle}
              onChange={(event) => {
                setName(event.target.value);
              }}
            />
          </Field>
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
          <Button type="button" onClick={submit}>
            Use this sender
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

interface SendTestEmailDialogProps {
  appId: string;
  defaultRecipient: string;
  draft: EmailTemplateDraft;
  onOpenChange: (open: boolean) => void;
}

function SendTestEmailDialog({
  appId,
  defaultRecipient,
  draft,
  onOpenChange,
}: SendTestEmailDialogProps) {
  const sendTest = useSendTestEmail(appId);
  const [recipient, setRecipient] = useState(defaultRecipient);
  const [error, setError] = useState<string>();

  async function send(): Promise<void> {
    const to = recipient.trim();
    if (!EMAIL_PATTERN.test(to)) {
      setError('Enter a valid email address.');
      return;
    }
    setError(undefined);
    try {
      const result = await sendTest.mutateAsync({
        to,
        subject: draft.subject,
        body: draft.body,
        senderName: draft.senderName,
        senderEmail: draft.senderEmail,
      });
      toastManager.add({
        type: 'success',
        title: `Test email sent to ${result['sent-to']}`,
      });
      onOpenChange(false);
    } catch (failure) {
      setError(
        failure instanceof Error
          ? failure.message
          : "Couldn't send the test email.",
      );
    }
  }

  return (
    <Dialog open onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Send test email</DialogTitle>
          <DialogDescription>
            Sends the template as it is, without saving. Only members of this
            app can receive it.
          </DialogDescription>
        </DialogHeader>
        <div className="mt-4">
          <Field>
            <FieldLabel htmlFor="test-email-recipient">Recipient</FieldLabel>
            <Input
              id="test-email-recipient"
              type="email"
              autoComplete="off"
              value={recipient}
              placeholder="you@example.com"
              aria-invalid={error !== undefined}
              onChange={(event) => {
                setRecipient(event.target.value);
                setError(undefined);
              }}
            />
          </Field>
          {error === undefined ? null : (
            <p role="alert" className="text-destructive mt-2 text-sm">
              {error}
            </p>
          )}
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
            disabled={sendTest.isPending}
            onClick={() => {
              void send();
            }}
          >
            {sendTest.isPending ? 'Sending…' : 'Send test email'}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

interface SenderCardProps {
  appId: string;
  appTitle: string;
  canManage: boolean;
  customEmail: string;
  customName: string;
  defaultSenderEmail: string;
  savedEmail: string;
  verification: SenderVerification | undefined;
  verificationLoading: boolean;
  onApply: (senderEmail: string, senderName: string) => void;
  onRemove: () => void;
}

function SenderCard({
  appId,
  appTitle,
  canManage,
  customEmail,
  customName,
  defaultSenderEmail,
  savedEmail,
  verification,
  verificationLoading,
  onApply,
  onRemove,
}: SenderCardProps) {
  const sendCode = useSendSenderVerificationCode(appId);
  const verifyCode = useVerifySenderCode(appId);
  const [dialogOpen, setDialogOpen] = useState(false);
  const [removeOpen, setRemoveOpen] = useState(false);
  const [code, setCode] = useState('');
  const [error, setError] = useState<string>();

  const draftEmail = customEmail.trim();
  const matchesSaved =
    draftEmail.toLowerCase() === savedEmail.trim().toLowerCase();
  const status = matchesSaved
    ? senderStatus(savedEmail, verification)
    : senderStatus(draftEmail, undefined);
  const addressVerified = verification?.instant['verified?'] === true;
  const needsVerification =
    draftEmail !== '' &&
    status.kind !== 'verified' &&
    status.kind !== 'default' &&
    !(status.kind === 'domain-disabled' && addressVerified);
  const normalizedCode = code.replace(/\D/g, '').slice(0, 6);

  async function sendVerificationCode(): Promise<void> {
    setError(undefined);
    try {
      await sendCode.mutateAsync();
      toastManager.add({
        type: 'success',
        title: `Verification code sent to ${draftEmail}`,
      });
    } catch (failure) {
      setError(
        failure instanceof Error
          ? failure.message
          : "Couldn't send the verification code.",
      );
    }
  }

  async function submitCode(): Promise<void> {
    if (normalizedCode.length !== 6) return;
    setError(undefined);
    try {
      const result = await verifyCode.mutateAsync(normalizedCode);
      if (result.verified) {
        setCode('');
        toastManager.add({
          type: 'success',
          title: 'Sender email verified',
        });
      } else {
        setError(
          result.error ??
            'The domain is not enabled for email sending in Cloudflare yet.',
        );
      }
    } catch (failure) {
      setError(
        failure instanceof Error
          ? failure.message
          : "Couldn't verify the code.",
      );
    }
  }

  return (
    <SectionCard
      title="Sender"
      description="Who the magic code email comes from."
      actions={
        status.kind === 'verified' ? (
          <Badge tone="success">Verified</Badge>
        ) : status.kind === 'pending' ? (
          <Badge tone="warning">Pending</Badge>
        ) : status.kind === 'domain-disabled' ? (
          <Badge tone="warning">Domain disabled</Badge>
        ) : (
          <Badge>Not set</Badge>
        )
      }
    >
      <div className="flex flex-col gap-4">
        {draftEmail === '' ? (
          <div className="flex flex-col gap-1">
            <p className="text-sm">
              BackendRelay default sender (
              <code className="bg-muted rounded px-1 py-0.5 font-mono text-xs">
                {defaultSenderEmail}
              </code>
              )
            </p>
            <p className="text-muted-foreground text-xs">
              Emails come from the shared BackendRelay address. Use your own
              domain to send from your address instead.
            </p>
          </div>
        ) : (
          <div className="flex flex-col gap-1">
            <p className="text-sm">
              Sending as{' '}
              <code className="bg-muted rounded px-1 py-0.5 font-mono text-xs">
                {draftEmail}
              </code>
            </p>
            <p className="text-muted-foreground text-xs">
              {effectiveSenderName(customName, appTitle)}
            </p>
          </div>
        )}

        {status.kind === 'domain-disabled' ? (
          <p className="text-warning text-sm" role="status">
            {status.label}
            {status.detail === undefined ? '' : ` (${status.detail})`}
          </p>
        ) : null}

        {verificationLoading && matchesSaved && draftEmail !== '' ? (
          <p className="text-muted-foreground text-sm" role="status">
            Checking verification status…
          </p>
        ) : null}

        {needsVerification ? (
          <div className="border-border flex flex-col gap-3 rounded-md border p-3">
            <p className="text-sm">
              {matchesSaved
                ? `Enter the 6-digit code sent to ${draftEmail}.`
                : 'Save the email settings to send a verification code to this address.'}
            </p>
            <div className="flex flex-col gap-2 sm:flex-row sm:items-center">
              <Button
                type="button"
                variant="secondary"
                size="sm"
                className="shrink-0"
                disabled={!canManage || !matchesSaved || sendCode.isPending}
                title={
                  matchesSaved ? undefined : 'Save the email settings first.'
                }
                onClick={() => {
                  void sendVerificationCode();
                }}
              >
                {sendCode.isPending ? 'Sending…' : 'Send code'}
              </Button>
              <Input
                aria-label="Verification code"
                inputMode="numeric"
                autoComplete="one-time-code"
                value={code}
                placeholder="123456"
                className="sm:max-w-32"
                onChange={(event) => {
                  setCode(event.target.value);
                  setError(undefined);
                }}
                onKeyDown={(event) => {
                  if (event.key === 'Enter') void submitCode();
                }}
              />
              <Button
                type="button"
                size="sm"
                className="shrink-0"
                disabled={
                  !canManage ||
                  !matchesSaved ||
                  normalizedCode.length !== 6 ||
                  verifyCode.isPending
                }
                onClick={() => {
                  void submitCode();
                }}
              >
                {verifyCode.isPending ? 'Verifying…' : 'Verify'}
              </Button>
            </div>
          </div>
        ) : null}

        {error === undefined ? null : (
          <p role="alert" className="text-destructive text-sm">
            {error}
          </p>
        )}

        <div className="flex flex-wrap items-center gap-2">
          <Button
            type="button"
            variant="secondary"
            size="sm"
            disabled={!canManage}
            title={
              canManage
                ? undefined
                : 'Only app administrators and owners can change the sender.'
            }
            onClick={() => {
              setDialogOpen(true);
            }}
          >
            <ShieldCheck aria-hidden="true" />
            {draftEmail === '' ? 'Use my own domain' : 'Change sender'}
          </Button>
          {draftEmail === '' ? null : (
            <Button
              type="button"
              variant="ghost"
              size="sm"
              className="text-destructive"
              disabled={!canManage}
              onClick={() => {
                setRemoveOpen(true);
              }}
            >
              Remove custom sender
            </Button>
          )}
        </div>
      </div>

      {dialogOpen ? (
        <SenderDialog
          appTitle={appTitle}
          senderEmail={customEmail}
          senderName={customName}
          onApply={onApply}
          onOpenChange={setDialogOpen}
        />
      ) : null}

      <ConfirmDialog
        open={removeOpen}
        onOpenChange={setRemoveOpen}
        title="Remove custom sender"
        description={`After you save, users receive email from the BackendRelay default sender (${defaultSenderEmail}).`}
        confirmLabel="Remove custom sender"
        onConfirm={() => {
          onRemove();
          setRemoveOpen(false);
        }}
      />
    </SectionCard>
  );
}

interface CodeExpiryCardProps {
  appId: string;
  expiryMinutes: number | null | undefined;
}

function CodeExpiryCard({ appId, expiryMinutes }: CodeExpiryCardProps) {
  const setExpiry = useSetMagicCodeExpiry(appId);
  const inputId = useId();
  const [value, setValue] = useState(
    String(effectiveExpiryMinutes(expiryMinutes)),
  );
  const problem = expiryMinutesProblem(value);
  const parsed = problem === undefined ? Number.parseInt(value, 10) : undefined;
  const dirty =
    parsed !== undefined && parsed !== effectiveExpiryMinutes(expiryMinutes);

  async function apply(): Promise<void> {
    if (parsed === undefined || setExpiry.isPending) return;
    try {
      await setExpiry.mutateAsync(parsed);
      toastManager.add({ type: 'success', title: 'Code expiry updated' });
    } catch {
      // The global mutation toast carries the server message.
    }
  }

  return (
    <SectionCard
      title="Code expiry"
      description="How long a magic code stays valid after it is sent."
    >
      <SettingsRow
        label="Codes expire after"
        htmlFor={inputId}
        description={`Between ${String(MAGIC_CODE_EXPIRY_MIN)} and ${String(MAGIC_CODE_EXPIRY_MAX)} minutes. Shorter lifetimes reduce the window for interception.`}
      >
        <div className="flex flex-col gap-1">
          <div className="flex items-center gap-2">
            <Input
              id={inputId}
              type="number"
              inputMode="numeric"
              min={MAGIC_CODE_EXPIRY_MIN}
              max={MAGIC_CODE_EXPIRY_MAX}
              value={value}
              className="w-24"
              aria-invalid={problem !== undefined}
              aria-describedby={
                problem === undefined ? undefined : `${inputId}-error`
              }
              onChange={(event) => {
                setValue(event.target.value);
              }}
            />
            <span className="text-muted-foreground text-sm">minutes</span>
            <Button
              type="button"
              size="sm"
              aria-label="Save code expiry"
              disabled={!dirty || problem !== undefined || setExpiry.isPending}
              onClick={() => {
                void apply();
              }}
            >
              {setExpiry.isPending ? 'Saving…' : 'Save'}
            </Button>
          </div>
          {problem === undefined ? null : (
            <p
              id={`${inputId}-error`}
              role="alert"
              className="text-destructive text-xs"
            >
              {problem}
            </p>
          )}
        </div>
      </SettingsRow>
    </SectionCard>
  );
}

/**
 * C.11 email tab: the magic-code template with a live preview, the sender and
 * its verification (P2.17), and the code expiry (P2.4). Email settings need the
 * admin role, which the server also enforces.
 */
export function EmailTab({
  appId,
  appTitle,
  role,
  defaultSenderEmail,
}: EmailTabProps) {
  const canManage = roleRank(role) >= roleRank('admin');
  const details = useAppDetails(appId);
  const session = useSession();
  const verification = useSenderVerification(appId, canManage);

  const save = useSaveEmailTemplate(appId);
  const deleteTemplate = useDeleteEmailTemplate(appId);
  const sendCode = useSendSenderVerificationCode(appId);

  const [draft, setDraft] = useState<EmailTemplateDraft>();
  const [saveError, setSaveError] = useState<string>();
  const [preview, setPreview] = useState(false);
  const [sendOpen, setSendOpen] = useState(false);
  const [resetOpen, setResetOpen] = useState(false);
  const bodyEditor = useRef<CodeEditorHandle>(null);

  const app = details.data?.app;
  const template = app?.magic_code_email_template;
  const title = app?.title ?? appTitle;
  const saved = emailTemplateDraft(template);
  const current = draft ?? saved;
  const dirty =
    draft !== undefined && emailTemplateDirty(current, template, title);
  const expiryMinutes = app?.magic_code_expiry_minutes;

  function updateDraft(update: Partial<EmailTemplateDraft>): void {
    setDraft({ ...current, ...update });
    setSaveError(undefined);
  }

  function insertVariable(variable: EmailTemplateVariable): void {
    const token = `{${variable}}`;
    const editor = bodyEditor.current;
    if (editor === null) {
      updateDraft({ body: `${current.body}${token}` });
      return;
    }
    editor.insertText(token);
  }

  function resetDraft(): void {
    setDraft(undefined);
    setSaveError(undefined);
    save.reset();
  }

  async function saveTemplate(): Promise<void> {
    if (save.isPending) return;
    const problem = emailTemplateProblem(current);
    if (problem !== undefined) {
      setSaveError(problem);
      return;
    }
    setSaveError(undefined);
    const senderEmail = current.senderEmail.trim();
    const senderChanged =
      senderEmail !== '' &&
      senderEmail.toLowerCase() !==
        (template?.email ?? '').trim().toLowerCase();
    try {
      await save.mutateAsync({
        subject: current.subject,
        body: current.body,
        senderName: current.senderName,
        senderEmail,
      });
      setDraft({
        senderName: current.senderName.trim(),
        senderEmail,
        subject: current.subject.trim(),
        body: current.body.trim(),
      });
      toastManager.add({ type: 'success', title: 'Email template saved' });
      if (senderChanged) {
        void verification.refetch();
        try {
          await sendCode.mutateAsync();
          toastManager.add({
            type: 'success',
            title: `Verification code sent to ${senderEmail}`,
          });
        } catch {
          // The global mutation toast carries the server message.
        }
      }
    } catch (failure) {
      setSaveError(
        failure instanceof Error
          ? failure.message
          : "Couldn't save the email template. Try again.",
      );
    }
  }

  async function resetTemplate(): Promise<void> {
    try {
      if (template?.id !== undefined) {
        await deleteTemplate.mutateAsync(template.id);
      }
      setDraft(undefined);
      setSaveError(undefined);
      setResetOpen(false);
      toastManager.add({
        type: 'success',
        title: 'Email template reset to the default',
      });
      void verification.refetch();
    } catch {
      // The global mutation toast carries the server message.
    }
  }

  if (!canManage) {
    return (
      <ErrorState
        thing="email settings"
        status={403}
        requiredRole="admin"
        message="Only app administrators and owners can change email settings."
      />
    );
  }

  if (details.isError && app === undefined) {
    return (
      <ErrorState
        thing="email settings"
        status={isApiError(details.error) ? details.error.status : undefined}
        message={
          details.error instanceof Error ? details.error.message : undefined
        }
        onRetry={() => {
          void details.refetch();
        }}
      />
    );
  }

  if (details.isPending && app === undefined) {
    return (
      <div className="flex flex-col gap-4">
        <CardSkeleton />
        <CardSkeleton />
      </div>
    );
  }

  const previewSubject = substituteSampleVars(current.subject, {
    appTitle: title,
    expiryMinutes,
  });
  const previewBody = substituteSampleVars(current.body, {
    appTitle: title,
    expiryMinutes,
    escape: true,
  });

  return (
    <div className="flex flex-col gap-4">
      <SectionCard
        title="Magic code email"
        description="The email users receive with their sign-in code."
        actions={
          <div className="flex items-center gap-2">
            <Button
              type="button"
              variant="secondary"
              size="sm"
              disabled={save.isPending}
              onClick={() => {
                setSendOpen(true);
              }}
            >
              <Send aria-hidden="true" />
              Send test email
            </Button>
            <Button
              type="button"
              variant="ghost"
              size="sm"
              className="text-destructive"
              disabled={save.isPending || deleteTemplate.isPending}
              onClick={() => {
                setResetOpen(true);
              }}
            >
              <RotateCcw aria-hidden="true" />
              Reset to default
            </Button>
          </div>
        }
      >
        <div className="flex flex-col gap-4">
          <div className="grid gap-4 sm:grid-cols-2">
            <Field>
              <FieldLabel htmlFor="magic-code-sender-name">
                Sender name
              </FieldLabel>
              <Input
                id="magic-code-sender-name"
                autoComplete="off"
                value={current.senderName}
                placeholder={title}
                disabled={save.isPending}
                onChange={(event) => {
                  updateDraft({ senderName: event.target.value });
                }}
              />
            </Field>
            <Field>
              <FieldLabel htmlFor="magic-code-sender-email">
                Sender email
              </FieldLabel>
              <Input
                id="magic-code-sender-email"
                type="email"
                autoComplete="off"
                value={current.senderEmail}
                placeholder={defaultSenderEmail}
                disabled={save.isPending}
                onChange={(event) => {
                  updateDraft({ senderEmail: event.target.value });
                }}
              />
              <FieldDescription>
                Leave blank to use the BackendRelay sender.
              </FieldDescription>
            </Field>
          </div>

          <Field>
            <FieldLabel htmlFor="magic-code-subject">Subject</FieldLabel>
            <Input
              id="magic-code-subject"
              autoComplete="off"
              value={current.subject}
              disabled={save.isPending}
              aria-invalid={!current.subject.includes('{code}')}
              onChange={(event) => {
                updateDraft({ subject: event.target.value });
              }}
            />
            <FieldDescription>
              Must include {'{code}'}; {'{app_title}'}, {'{user_email}'} and{' '}
              {'{expiration}'} also work.
            </FieldDescription>
          </Field>

          <div className="flex flex-col gap-2">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <div className="flex items-center gap-2">
                <p className="text-sm font-medium">Body</p>
                <div className="flex items-center gap-1">
                  <Button
                    type="button"
                    variant={preview ? 'ghost' : 'secondary'}
                    size="sm"
                    aria-pressed={!preview}
                    onClick={() => {
                      setPreview(false);
                    }}
                  >
                    Edit
                  </Button>
                  <Button
                    type="button"
                    variant={preview ? 'secondary' : 'ghost'}
                    size="sm"
                    aria-pressed={preview}
                    onClick={() => {
                      setPreview(true);
                    }}
                  >
                    Preview
                  </Button>
                </div>
              </div>
              <div className="flex flex-wrap items-center gap-1.5 text-xs">
                <span className="text-muted-foreground">Variables:</span>
                {EMAIL_TEMPLATE_VARIABLES.map((variable) => (
                  <Button
                    key={variable}
                    type="button"
                    variant="secondary"
                    size="sm"
                    className="h-6 px-1.5 font-mono text-xs"
                    aria-label={`Insert {${variable}}`}
                    disabled={preview || save.isPending}
                    onClick={() => {
                      insertVariable(variable);
                    }}
                  >
                    {`{${variable}}`}
                  </Button>
                ))}
              </div>
            </div>

            {preview ? (
              <div className="border-border overflow-hidden rounded-md border">
                <div className="border-border bg-muted/40 flex flex-col gap-0.5 border-b px-3 py-2 text-sm">
                  <div className="flex gap-2">
                    <span className="text-muted-foreground w-14 shrink-0">
                      From
                    </span>
                    <span className="truncate">
                      {effectiveSenderName(current.senderName, title)}
                    </span>
                  </div>
                  <div className="flex gap-2">
                    <span className="text-muted-foreground w-14 shrink-0">
                      Subject
                    </span>
                    <span className="truncate font-medium">
                      {previewSubject === '' ? '—' : previewSubject}
                    </span>
                  </div>
                </div>
                <iframe
                  title="Email preview"
                  sandbox=""
                  srcDoc={previewBody}
                  className="h-[28rem] w-full bg-white"
                />
              </div>
            ) : (
              <CodeEditor
                value={current.body}
                onChange={(body) => {
                  updateDraft({ body });
                }}
                mode="html"
                aria-label="Email body"
                handleRef={bodyEditor}
                readOnly={save.isPending}
                height="28rem"
              />
            )}
            {preview ? (
              <p className="text-muted-foreground text-xs">
                A live approximation. Send a test to see it in a real inbox.
              </p>
            ) : null}
          </div>
        </div>
      </SectionCard>

      <SenderCard
        appId={appId}
        appTitle={title}
        canManage={canManage}
        customEmail={current.senderEmail}
        customName={current.senderName}
        defaultSenderEmail={defaultSenderEmail}
        savedEmail={template?.email ?? ''}
        verification={verification.data}
        verificationLoading={verification.isFetching}
        onApply={(senderEmail, senderName) => {
          updateDraft({ senderEmail, senderName });
        }}
        onRemove={() => {
          updateDraft({ senderEmail: '', senderName: '' });
        }}
      />

      <CodeExpiryCard appId={appId} expiryMinutes={expiryMinutes} />

      <UnsavedChangesBar
        dirty={dirty}
        saving={save.isPending}
        error={saveError ?? null}
        onReset={resetDraft}
        onSave={() => {
          void saveTemplate();
        }}
      />

      {sendOpen ? (
        <SendTestEmailDialog
          appId={appId}
          defaultRecipient={session?.user.email ?? ''}
          draft={current}
          onOpenChange={setSendOpen}
        />
      ) : null}

      <ConfirmDialog
        open={resetOpen}
        onOpenChange={setResetOpen}
        title="Reset to the default email?"
        description="This deletes the customized sender, subject and body. Users receive the BackendRelay default email until you customize it again."
        confirmLabel="Reset template"
        confirming={deleteTemplate.isPending}
        onConfirm={() => {
          void resetTemplate();
        }}
      />
    </div>
  );
}
