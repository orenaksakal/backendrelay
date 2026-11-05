import type { SenderVerification } from '@/lib/api/endpoints';
import type { App } from '@/lib/api/endpoints/schemas';

/**
 * C.11 email tab rules: the built-in magic-code template, its variables, the
 * draft/dirty comparison and the sender status derived from the verification
 * payload. Everything here is pure so the tab and its tests share one source.
 */

export const DEFAULT_MAGIC_CODE_SUBJECT = '{code} is your code for {app_title}';

/** Matches the server's fallback template closely enough for a live preview. */
export const DEFAULT_MAGIC_CODE_BODY = /* html */ `<div style="background: #f6f6f6; font-family: Helvetica, Arial, sans-serif; line-height: 1.6; font-size: 18px;">
  <div style="max-width: 650px; margin: 0 auto; background: white; padding: 20px;">
    <p><strong>Welcome,</strong></p>
    <p>
      You asked to join {app_title}. To complete your registration, use this
      verification code:
    </p>
    <h2 style="text-align: center"><strong>{code}</strong></h2>
    <p>
      Copy and paste this into the confirmation box, and you'll be on your way.
    </p>
    <p>
      Note: This code will expire in {expiration}, and can only be used once. If
      you didn't request this code, please reply to this email.
    </p>
  </div>
</div>
`;

/** The exact list the old template editor offered. */
export const EMAIL_TEMPLATE_VARIABLES = [
  'code',
  'app_title',
  'user_email',
  'expiration',
] as const;

export type EmailTemplateVariable = (typeof EMAIL_TEMPLATE_VARIABLES)[number];

/** P2.4: the server default and the per-app override cap. */
export const DEFAULT_MAGIC_CODE_EXPIRY_MINUTES = 15;
export const MAGIC_CODE_EXPIRY_MIN = 1;
export const MAGIC_CODE_EXPIRY_MAX = 60;

export interface EmailTemplateDraft {
  senderName: string;
  senderEmail: string;
  subject: string;
  body: string;
}

type EmailTemplate = App['magic_code_email_template'];

/** Seeds the form from the stored template, or from the platform defaults. */
export function emailTemplateDraft(
  template: EmailTemplate,
): EmailTemplateDraft {
  return {
    senderName: template?.name ?? '',
    senderEmail: template?.email ?? '',
    subject: template?.subject ?? DEFAULT_MAGIC_CODE_SUBJECT,
    body: template?.body ?? DEFAULT_MAGIC_CODE_BODY,
  };
}

/** An empty sender name means "use the app title", as the server does. */
export function effectiveSenderName(
  senderName: string,
  appTitle: string,
): string {
  const trimmed = senderName.trim();
  return trimmed === '' ? appTitle : trimmed;
}

/**
 * Whether the draft differs from the saved template. The comparison is
 * normalized so clearing a sender name back to the app title is not dirty, and
 * sender emails compare case-insensitively.
 */
export function emailTemplateDirty(
  draft: EmailTemplateDraft,
  template: EmailTemplate,
  appTitle: string,
): boolean {
  const saved = emailTemplateDraft(template);
  return (
    effectiveSenderName(draft.senderName, appTitle) !==
      effectiveSenderName(saved.senderName, appTitle) ||
    draft.senderEmail.trim().toLowerCase() !==
      saved.senderEmail.trim().toLowerCase() ||
    draft.subject !== saved.subject ||
    draft.body !== saved.body
  );
}

/** The server rejects templates without `{code}`; surface that before saving. */
export function emailTemplateProblem(
  draft: EmailTemplateDraft,
): string | undefined {
  if (draft.subject.trim() === '') {
    return 'Enter a subject.';
  }
  if (!draft.subject.includes('{code}')) {
    return 'The subject must include {code}.';
  }
  if (draft.body.trim() === '') {
    return 'Enter an email body.';
  }
  if (!draft.body.includes('{code}')) {
    return 'The body must include {code}.';
  }
  return undefined;
}

export function escapeHtml(value: string): string {
  return value
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;')
    .replaceAll("'", '&#39;');
}

export interface SampleVariables {
  appTitle: string;
  expiryMinutes: number | null | undefined;
  /** HTML-escape the substituted values (for the iframe preview). */
  escape?: boolean;
}

/** Fills variables with sample values the same way the server does. */
export function substituteSampleVars(
  template: string,
  { appTitle, expiryMinutes, escape = false }: SampleVariables,
): string {
  const value = (raw: string) => (escape ? escapeHtml(raw) : raw);
  return template
    .replaceAll('{code}', value('123456'))
    .replaceAll('{app_title}', value(appTitle))
    .replaceAll('{user_email}', value('happyuser@gmail.com'))
    .replaceAll('{expiration}', value(expiryLabel(expiryMinutes)));
}

/** `{expiration}` wording; matches `render_test_template` on the server. */
export function expiryLabel(minutes: number | null | undefined): string {
  const effective = effectiveExpiryMinutes(minutes);
  if (effective >= 60) {
    const hours = Math.floor(effective / 60);
    return `${String(hours)} hour${hours > 1 ? 's' : ''}`;
  }
  return `${String(effective)} minute${effective > 1 ? 's' : ''}`;
}

export function effectiveExpiryMinutes(
  minutes: number | null | undefined,
): number {
  if (minutes === null || minutes === undefined) {
    return DEFAULT_MAGIC_CODE_EXPIRY_MINUTES;
  }
  return Math.min(
    Math.max(minutes, MAGIC_CODE_EXPIRY_MIN),
    MAGIC_CODE_EXPIRY_MAX,
  );
}

/** Returns the parsed minutes, or a message for the invalid input. */
export function expiryMinutesProblem(raw: string): string | undefined {
  const trimmed = raw.trim();
  if (!/^\d+$/.test(trimmed)) {
    return 'Enter a whole number of minutes.';
  }
  const minutes = Number.parseInt(trimmed, 10);
  if (minutes < MAGIC_CODE_EXPIRY_MIN || minutes > MAGIC_CODE_EXPIRY_MAX) {
    return `Choose between ${String(MAGIC_CODE_EXPIRY_MIN)} and ${String(MAGIC_CODE_EXPIRY_MAX)} minutes.`;
  }
  return undefined;
}

export type SenderStatusKind =
  | 'default'
  | 'pending'
  | 'verified'
  | 'domain-disabled';

export interface SenderStatus {
  kind: SenderStatusKind;
  label: string;
  /** The provider's reason, when it has one. */
  detail?: string;
}

/**
 * Collapses the verification payload into the four states C.11 shows. Postmark
 * requires both the emailed code and its own sender confirmation; Cloudflare
 * requires the emailed code plus an enabled sending domain (P2.17).
 */
export function senderStatus(
  senderEmail: string | null | undefined,
  verification: SenderVerification | undefined,
): SenderStatus {
  const email = senderEmail?.trim() ?? '';
  if (email === '') {
    return { kind: 'default', label: 'Not set' };
  }
  if (verification === undefined) {
    return { kind: 'pending', label: 'Pending' };
  }

  const domain = verification.domain ?? null;
  if (domain?.cloudflare_enabled === false) {
    return {
      kind: 'domain-disabled',
      label: 'Domain no longer enabled — using the default sender',
      detail: domain.last_error ?? undefined,
    };
  }

  const instantVerified = verification.instant['verified?'] === true;
  const postmark = verification.verification ?? null;
  if (instantVerified && (postmark === null || postmark.Confirmed === true)) {
    return { kind: 'verified', label: 'Verified' };
  }
  return {
    kind: 'pending',
    label: 'Pending',
    detail:
      postmark?.Confirmed === false
        ? 'Waiting for sender confirmation.'
        : undefined,
  };
}
