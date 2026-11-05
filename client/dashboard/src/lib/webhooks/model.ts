import type { BadgeProps } from '@/components/ui/badge';
import type {
  Webhook,
  WebhookAction,
  WebhookAttempt,
  WebhookEvent,
} from '@/lib/api/endpoints';

/**
 * C.13 webhooks: URL checks that mirror the server's SSRF guard, the form
 * draft, delivery status labels and the signing helpers. Everything here is
 * pure so the sheet, the tables and the tests share one set of rules.
 */

/** The server refuses a cast that would exceed this many active webhooks. */
export const MAX_ACTIVE_WEBHOOKS = 100;

/** Events older than this cannot be resent (C.13). */
export const RESEND_WINDOW_MS = 30 * 24 * 60 * 60 * 1000;

/** How often the deliveries table refreshes while a row is in flight. */
export const DELIVERY_POLL_MS = 5000;

/** Canonical order for action checkboxes, badges and request bodies. */
export const WEBHOOK_ACTIONS: readonly WebhookAction[] = [
  'create',
  'update',
  'delete',
];

export const ACTION_LABELS: Record<WebhookAction, string> = {
  create: 'Create',
  update: 'Update',
  delete: 'Delete',
};

export const ACTION_TONES: Record<
  WebhookAction,
  NonNullable<BadgeProps['tone']>
> = {
  create: 'success',
  update: 'accent',
  delete: 'destructive',
};

const PRIVATE_IPV4_RANGES: ReadonlyArray<readonly [string, number]> = [
  ['0.0.0.0', 8],
  ['10.0.0.0', 8],
  ['100.64.0.0', 10],
  ['127.0.0.0', 8],
  ['169.254.0.0', 16],
  ['172.16.0.0', 12],
  ['192.0.0.0', 24],
  ['192.168.0.0', 16],
  ['198.18.0.0', 15],
  ['224.0.0.0', 4],
  ['240.0.0.0', 4],
];

/** Hostnames the server's allowlist check always rejects. */
const PRIVATE_HOSTNAMES = new Set(['localhost', 'localhost.localdomain']);
const PRIVATE_HOST_SUFFIXES = [
  '.localhost',
  '.local',
  '.internal',
  '.home.arpa',
];

function ipv4Parts(host: string): number[] | undefined {
  const match = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.exec(host);
  if (match === null) return undefined;
  const parts = match.slice(1).map((part) => Number(part));
  return parts.length === 4 && parts.every((part) => part <= 255)
    ? parts
    : undefined;
}

function ipv4ToInt(parts: readonly number[]): number {
  const [a = 0, b = 0, c = 0, d = 0] = parts;
  return ((a << 24) | (b << 16) | (c << 8) | d) >>> 0;
}

function ipv4InRange(
  parts: readonly number[],
  base: string,
  bits: number,
): boolean {
  const baseParts = ipv4Parts(base);
  if (baseParts === undefined) return false;
  const mask = bits === 0 ? 0 : (0xffffffff << (32 - bits)) >>> 0;
  return (ipv4ToInt(parts) & mask) === (ipv4ToInt(baseParts) & mask);
}

/**
 * IPv4-mapped IPv6 (`::ffff:10.0.0.1`) and link-local/ULA/loopback literals.
 * `host` has no brackets.
 */
function isPrivateIpv6(host: string): boolean {
  const lower = host.toLowerCase();
  if (lower === '::' || lower === '::1') return true;
  const mapped = /^::ffff:(\d{1,3}(?:\.\d{1,3}){3})$/.exec(lower);
  if (mapped !== null) {
    const parts = ipv4Parts(mapped[1] ?? '');
    return parts === undefined || isPrivateIpv4(parts);
  }
  const mappedHex = /^::ffff:([0-9a-f]{1,4}):([0-9a-f]{1,4})$/.exec(lower);
  if (mappedHex !== null) {
    const high = Number.parseInt(mappedHex[1] ?? '', 16);
    const low = Number.parseInt(mappedHex[2] ?? '', 16);
    return (
      !Number.isFinite(high) ||
      !Number.isFinite(low) ||
      isPrivateIpv4([high >> 8, high & 0xff, low >> 8, low & 0xff])
    );
  }
  const firstGroup = lower.split(':')[0] ?? '';
  const first = Number.parseInt(firstGroup, 16);
  if (!Number.isFinite(first)) return false;
  // fc00::/7 unique-local and fe80::/10 link-local.
  return (first & 0xfe00) === 0xfc00 || (first & 0xffc0) === 0xfe80;
}

function isPrivateIpv4(parts: readonly number[]): boolean {
  return PRIVATE_IPV4_RANGES.some(([base, bits]) =>
    ipv4InRange(parts, base, bits),
  );
}

/** True for loopback, link-local, CGNAT and private literals and names. */
export function isPublicHost(hostname: string): boolean {
  let host = hostname.trim().toLowerCase();
  if (host.startsWith('[') && host.endsWith(']')) {
    host = host.slice(1, -1);
  }
  if (host.endsWith('.')) host = host.slice(0, -1);
  if (host === '') return false;
  if (PRIVATE_HOSTNAMES.has(host)) return false;
  if (PRIVATE_HOST_SUFFIXES.some((suffix) => host.endsWith(suffix))) {
    return false;
  }
  const ipv4 = ipv4Parts(host);
  if (ipv4 !== undefined) return !isPrivateIpv4(ipv4);
  if (host.includes(':')) return !isPrivateIpv6(host);
  // A single-label name only resolves inside a private network.
  return host.includes('.');
}

/** Inline validation for the URL field (C.13). */
export function webhookUrlProblem(value: string): string | undefined {
  const trimmed = value.trim();
  if (trimmed === '') return 'Enter a webhook URL.';
  let url: URL;
  try {
    url = new URL(trimmed);
  } catch {
    return 'Enter a valid URL.';
  }
  if (url.protocol !== 'https:') return 'The webhook URL must use https.';
  if (!isPublicHost(url.hostname)) {
    return 'The webhook URL must point at a public host.';
  }
  return undefined;
}

export interface WebhookDraft {
  url: string;
  namespaces: string[];
  actions: WebhookAction[];
}

/** Checkboxes and badges always read create → update → delete. */
export function normalizeActions(
  actions: readonly WebhookAction[],
): WebhookAction[] {
  return WEBHOOK_ACTIONS.filter((action) => actions.includes(action));
}

export function sortedNamespaces(
  namespaces: Iterable<string> | null | undefined,
): string[] {
  return [...new Set(namespaces ?? [])].sort((a, b) =>
    a < b ? -1 : a > b ? 1 : 0,
  );
}

export function webhookDraft(webhook?: Webhook): WebhookDraft {
  return {
    url: webhook?.sink.url ?? '',
    namespaces: sortedNamespaces(webhook?.namespaces),
    actions: normalizeActions(webhook?.actions ?? ['create']),
  };
}

export function webhookDraftProblem(draft: WebhookDraft): string | undefined {
  const urlProblem = webhookUrlProblem(draft.url);
  if (urlProblem !== undefined) return urlProblem;
  if (draft.namespaces.length === 0) return 'Select at least one namespace.';
  if (draft.actions.length === 0) return 'Select at least one action.';
  return undefined;
}

export function webhookBody(draft: WebhookDraft): WebhookDraft {
  return {
    url: draft.url.trim(),
    namespaces: sortedNamespaces(draft.namespaces),
    actions: normalizeActions(draft.actions),
  };
}

/**
 * Schema namespaces plus any namespace the webhook already points at, so a
 * renamed or deleted table can still be deselected (old dashboard behaviour).
 */
export function namespaceOptions(
  schemaNamespaces: readonly string[],
  selected: readonly string[],
): string[] {
  return sortedNamespaces([...schemaNamespaces, ...selected]);
}

export function activeWebhookCount(webhooks: readonly Webhook[]): number {
  return webhooks.filter((webhook) => webhook.status === 'active').length;
}

export function activeWebhookLimitReached(
  webhooks: readonly Webhook[],
): boolean {
  return activeWebhookCount(webhooks) >= MAX_ACTIVE_WEBHOOKS;
}

export function webhookLimitNote(webhooks: readonly Webhook[]): string {
  return `${String(activeWebhookCount(webhooks))} of ${String(MAX_ACTIVE_WEBHOOKS)} active webhooks used.`;
}

/** Newest first, matching the server's list order. */
export function sortedWebhooks(webhooks: readonly Webhook[]): Webhook[] {
  return [...webhooks].sort((a, b) => {
    const byCreated = b.created_at.localeCompare(a.created_at);
    return byCreated !== 0 ? byCreated : a.id.localeCompare(b.id);
  });
}

export function namespacesForDisplay(
  namespaces: readonly string[] | null | undefined,
  limit = 3,
): { shown: string[]; extra: number } {
  const all = sortedNamespaces(namespaces);
  return { shown: all.slice(0, limit), extra: Math.max(0, all.length - limit) };
}

export interface DeliveryDisplay {
  label: 'Delivered' | 'Pending' | 'Failed';
  tone: NonNullable<BadgeProps['tone']>;
}

/** The event statuses are `pending | processing | success | error | failed`. */
export function deliveryDisplay(status: string): DeliveryDisplay {
  switch (status) {
    case 'success':
      return { label: 'Delivered', tone: 'success' };
    case 'error':
    case 'failed':
      return { label: 'Failed', tone: 'destructive' };
    default:
      return { label: 'Pending', tone: 'warning' };
  }
}

export function isEventInFlight(event: WebhookEvent): boolean {
  return event.status === 'pending' || event.status === 'processing';
}

export function anyEventInFlight(events: readonly WebhookEvent[]): boolean {
  return events.some(isEventInFlight);
}

/** Attempts arrive in the order they ran; the UI reads the newest first. */
export function attemptAt(attempt: WebhookAttempt): string | undefined {
  return attempt['attempt-at'] ?? attempt.attempt_at ?? undefined;
}

export function attemptDuration(attempt: WebhookAttempt): number | undefined {
  return attempt['duration-ms'] ?? attempt.duration_ms ?? undefined;
}

export function attemptSucceeded(attempt: WebhookAttempt): boolean | undefined {
  return attempt['success?'] ?? attempt.success ?? undefined;
}

export function attemptStatusCode(attempt: WebhookAttempt): number | undefined {
  return attempt['status-code'] ?? attempt.status_code ?? undefined;
}

export function attemptResponseText(
  attempt: WebhookAttempt,
): string | undefined {
  return attempt['response-text'] ?? attempt.response_text ?? undefined;
}

export function attemptErrorType(attempt: WebhookAttempt): string | undefined {
  return attempt['error-type'] ?? attempt.error_type ?? undefined;
}

export function attemptErrorMessage(
  attempt: WebhookAttempt,
): string | undefined {
  return attempt['error-message'] ?? attempt.error_message ?? undefined;
}

export function attemptsNewestFirst(attempt: WebhookEvent): WebhookAttempt[] {
  const attempts = attempt.attempts ?? [];
  return [...attempts].sort((a, b) =>
    (attemptAt(b) ?? '').localeCompare(attemptAt(a) ?? ''),
  );
}

export function latestAttempt(event: WebhookEvent): WebhookAttempt | undefined {
  return attemptsNewestFirst(event)[0];
}

export function deliveryAttemptCount(event: WebhookEvent): number {
  return event.attempts?.length ?? 0;
}

export function deliveryHttpCode(event: WebhookEvent): number | undefined {
  const attempt = latestAttempt(event);
  return attempt === undefined ? undefined : attemptStatusCode(attempt);
}

export function deliveryDuration(event: WebhookEvent): number | undefined {
  const attempt = latestAttempt(event);
  return attempt === undefined ? undefined : attemptDuration(attempt);
}

/** The newest page-one event, used for the list's last-delivery column. */
export function latestEvent(
  events: readonly WebhookEvent[],
): WebhookEvent | undefined {
  let newest: WebhookEvent | undefined;
  for (const event of events) {
    if (
      newest === undefined ||
      event.created_at > newest.created_at ||
      (event.created_at === newest.created_at && event.isn > newest.isn)
    ) {
      newest = event;
    }
  }
  return newest;
}

export function canResendEvent(
  event: WebhookEvent,
  now: Date = new Date(),
): boolean {
  const createdAt = new Date(event.created_at).getTime();
  if (Number.isNaN(createdAt)) return false;
  return now.getTime() - createdAt <= RESEND_WINDOW_MS;
}

/** `${apiBaseUrl}/.well-known/webhooks/jwks.json` (C.13). */
export function jwksUrl(apiBaseUrl: string): string {
  return `${apiBaseUrl.replace(/\/+$/, '')}/.well-known/webhooks/jwks.json`;
}

export interface VerificationSnippetOptions {
  appId: string;
  apiBaseUrl: string;
}

/** The `@instantdb/webhooks` handler the Signing card shows. */
export function verificationSnippet({
  appId,
  apiBaseUrl,
}: VerificationSnippetOptions): string {
  return `import { Webhooks } from '@instantdb/webhooks';

const webhooks = new Webhooks({
  appId: '${appId}',
  apiURI: '${apiBaseUrl.replace(/\/+$/, '')}',
});

export async function POST(request: Request) {
  const body = await webhooks.validateRequest(request);
  const payload = await webhooks.fetchPayloads(body);
  await webhooks.processPayload(handlers, payload);
  return new Response(null, { status: 204 });
}`;
}
