import type { KnownOperatorFlag, RestoreJob } from '@/lib/api/endpoints';

/**
 * C.19 operator settings. The typed groups and the advanced table share one
 * draft so the page has a single save model (B.5.1): every control writes into
 * the draft and the `UnsavedChangesBar` persists the whole change set.
 */

export const FLAG_SIGNUPS = 'dashboard-signups';
export const FLAG_EPHEMERAL_APPS = 'ephemeral-apps-enabled';
export const FLAG_MAGIC_CODE_RATE = 'magic-code-rate-limit-per-hour';
export const FLAG_MAGIC_CODE_EXPIRY = 'default-magic-code-expiry-minutes';
export const FLAG_WEBHOOK_IPS = 'smokescreen-whitelist-ips';
export const FLAG_THROTTLED_APPS = 'refresh-throttled-apps';
export const FLAG_THROTTLE_MS = 'refresh-throttle-ms';

/** Flags with a typed control; the advanced table lists everything else. */
export const MANAGED_FLAG_NAMES = [
  FLAG_SIGNUPS,
  FLAG_EPHEMERAL_APPS,
  FLAG_MAGIC_CODE_RATE,
  FLAG_MAGIC_CODE_EXPIRY,
  FLAG_WEBHOOK_IPS,
  FLAG_THROTTLED_APPS,
  FLAG_THROTTLE_MS,
] as const;

export const SIGNUP_MODES = ['open', 'restricted', 'closed'] as const;
export type SignupMode = (typeof SIGNUP_MODES)[number];

export interface SignupModeOption {
  value: SignupMode;
  label: string;
  description: string;
}

export const SIGNUP_MODE_OPTIONS: ReadonlyArray<SignupModeOption> = [
  {
    value: 'open',
    label: 'Open',
    description: 'Anyone can create a BackendRelay account.',
  },
  {
    value: 'restricted',
    label: 'Restricted',
    description: 'Only the email addresses below can create an account.',
  },
  {
    value: 'closed',
    label: 'Closed',
    description: 'No new accounts. Existing users can still sign in.',
  },
];

/** Fallbacks that match the Rust server when a flag is unset. */
export const DEFAULT_MAGIC_CODE_RATE_LIMIT = 20;
export const DEFAULT_MAGIC_CODE_EXPIRY_MINUTES = 15;
export const DEFAULT_REFRESH_THROTTLE_MS = 1000;

export type AdvancedFlagChange =
  | { action: 'set'; value: unknown }
  | { action: 'reset' };

export interface OperatorSettingsDraft {
  signupMode: SignupMode;
  /** One address per line, so partial input stays visible while typing. */
  allowedEmailsText: string;
  ephemeralApps: boolean;
  magicCodeRateLimit: string;
  magicCodeExpiry: string;
  /** One IP address or CIDR per line. */
  webhookIpsText: string;
  throttledApps: string[];
  throttleMs: string;
  /** Staged advanced edits, applied by the same save. */
  advanced: Record<string, AdvancedFlagChange>;
}

export interface SignupPolicy {
  mode: SignupMode;
  allowedEmails: string[];
}

export interface OperatorFlagWrite {
  name: string;
  value: unknown;
}

export interface OperatorSavePlan {
  writes: OperatorFlagWrite[];
  resets: string[];
}

export interface OperatorDraftProblems {
  allowedEmails?: string;
  magicCodeRateLimit?: string;
  magicCodeExpiry?: string;
  webhookIps?: string;
  throttleMs?: string;
}

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

export function isSignupMode(value: unknown): value is SignupMode {
  return (
    typeof value === 'string' &&
    (SIGNUP_MODES as ReadonlyArray<string>).includes(value)
  );
}

/** Trims, lowercases, de-duplicates and sorts a string list flag. */
export function readStringList(value: unknown): string[] {
  if (!Array.isArray(value)) return [];
  return normaliseList(value.filter(isString));
}

export function parseLineList(text: string): string[] {
  return normaliseList(text.split(/\r?\n/));
}

function normaliseList(values: ReadonlyArray<string>): string[] {
  const unique = new Set<string>();
  for (const value of values) {
    const normalised = value.trim().toLowerCase();
    if (normalised !== '') unique.add(normalised);
  }
  return [...unique].sort((left, right) => left.localeCompare(right));
}

export function readSignups(value: unknown): SignupPolicy {
  if (!isRecord(value)) return { mode: 'open', allowedEmails: [] };
  return {
    mode: isSignupMode(value['mode']) ? value['mode'] : 'open',
    allowedEmails: readStringList(value['allowedEmails']),
  };
}

export function readIntegerFlag(
  value: unknown,
  fallback: number,
  minimum: number,
): number {
  return typeof value === 'number' &&
    Number.isSafeInteger(value) &&
    value >= minimum
    ? value
    : fallback;
}

export function readBooleanFlag(value: unknown, fallback: boolean): boolean {
  return typeof value === 'boolean' ? value : fallback;
}

export function parseIntegerText(
  text: string,
  minimum: number,
): number | undefined {
  const trimmed = text.trim();
  if (!/^\d+$/.test(trimmed)) return undefined;
  const parsed = Number(trimmed);
  return Number.isSafeInteger(parsed) && parsed >= minimum ? parsed : undefined;
}

export function draftFromFlags(
  flags: Record<string, unknown>,
): OperatorSettingsDraft {
  const signups = readSignups(flags[FLAG_SIGNUPS]);
  return {
    signupMode: signups.mode,
    allowedEmailsText: signups.allowedEmails.join('\n'),
    ephemeralApps: readBooleanFlag(flags[FLAG_EPHEMERAL_APPS], true),
    magicCodeRateLimit: String(
      readIntegerFlag(
        flags[FLAG_MAGIC_CODE_RATE],
        DEFAULT_MAGIC_CODE_RATE_LIMIT,
        1,
      ),
    ),
    magicCodeExpiry: String(
      readIntegerFlag(
        flags[FLAG_MAGIC_CODE_EXPIRY],
        DEFAULT_MAGIC_CODE_EXPIRY_MINUTES,
        1,
      ),
    ),
    webhookIpsText: readStringList(flags[FLAG_WEBHOOK_IPS]).join('\n'),
    throttledApps: readStringList(flags[FLAG_THROTTLED_APPS]),
    throttleMs: String(
      readIntegerFlag(flags[FLAG_THROTTLE_MS], DEFAULT_REFRESH_THROTTLE_MS, 0),
    ),
    advanced: {},
  };
}

export function operatorDraftProblems(
  draft: OperatorSettingsDraft,
): OperatorDraftProblems {
  const problems: OperatorDraftProblems = {};

  if (parseIntegerText(draft.magicCodeRateLimit, 1) === undefined) {
    problems.magicCodeRateLimit = 'Use a whole number of 1 or more.';
  }

  const expiry = parseIntegerText(draft.magicCodeExpiry, 1);
  if (expiry === undefined) {
    problems.magicCodeExpiry = 'Use a whole number of 1 or more.';
  }

  if (parseIntegerText(draft.throttleMs, 0) === undefined) {
    problems.throttleMs = 'Use a whole number of 0 or more.';
  }

  if (draft.signupMode === 'restricted') {
    const invalid = parseLineList(draft.allowedEmailsText).find(
      (email) => !isEmail(email),
    );
    if (invalid !== undefined) {
      problems.allowedEmails = `"${invalid}" isn't an email address.`;
    }
  }

  const invalidAddress = parseLineList(draft.webhookIpsText).find(
    (address) => !isIpOrCidr(address),
  );
  if (invalidAddress !== undefined) {
    problems.webhookIps = `"${invalidAddress}" isn't an IP address or CIDR.`;
  }

  return problems;
}

export function hasDraftProblems(problems: OperatorDraftProblems): boolean {
  return Object.keys(problems).length > 0;
}

/**
 * The PUT/DELETE sequence for one save. Only values that differ from the
 * server's copy are included, so a no-op save sends no requests.
 */
export function savePlanFromDraft(
  initialFlags: Record<string, unknown>,
  draft: OperatorSettingsDraft,
): OperatorSavePlan {
  const writes: OperatorFlagWrite[] = [];
  const resets: string[] = [];

  const initialSignups = readSignups(initialFlags[FLAG_SIGNUPS]);
  const nextSignups: SignupPolicy = {
    mode: draft.signupMode,
    allowedEmails: parseLineList(draft.allowedEmailsText),
  };
  if (
    initialSignups.mode !== nextSignups.mode ||
    !sameList(initialSignups.allowedEmails, nextSignups.allowedEmails)
  ) {
    writes.push({ name: FLAG_SIGNUPS, value: nextSignups });
  }

  if (
    readBooleanFlag(initialFlags[FLAG_EPHEMERAL_APPS], true) !==
    draft.ephemeralApps
  ) {
    writes.push({
      name: FLAG_EPHEMERAL_APPS,
      value: draft.ephemeralApps,
    });
  }

  const rate = parseIntegerText(draft.magicCodeRateLimit, 1);
  if (
    rate !== undefined &&
    rate !==
      readIntegerFlag(
        initialFlags[FLAG_MAGIC_CODE_RATE],
        DEFAULT_MAGIC_CODE_RATE_LIMIT,
        1,
      )
  ) {
    writes.push({ name: FLAG_MAGIC_CODE_RATE, value: rate });
  }

  const expiry = parseIntegerText(draft.magicCodeExpiry, 1);
  if (
    expiry !== undefined &&
    expiry !==
      readIntegerFlag(
        initialFlags[FLAG_MAGIC_CODE_EXPIRY],
        DEFAULT_MAGIC_CODE_EXPIRY_MINUTES,
        1,
      )
  ) {
    writes.push({ name: FLAG_MAGIC_CODE_EXPIRY, value: expiry });
  }

  const webhookIps = parseLineList(draft.webhookIpsText);
  if (!sameList(readStringList(initialFlags[FLAG_WEBHOOK_IPS]), webhookIps)) {
    writes.push({ name: FLAG_WEBHOOK_IPS, value: webhookIps });
  }

  const throttledApps = normaliseList(draft.throttledApps);
  if (
    !sameList(readStringList(initialFlags[FLAG_THROTTLED_APPS]), throttledApps)
  ) {
    writes.push({ name: FLAG_THROTTLED_APPS, value: throttledApps });
  }

  const throttleMs = parseIntegerText(draft.throttleMs, 0);
  if (
    throttleMs !== undefined &&
    throttleMs !==
      readIntegerFlag(
        initialFlags[FLAG_THROTTLE_MS],
        DEFAULT_REFRESH_THROTTLE_MS,
        0,
      )
  ) {
    writes.push({ name: FLAG_THROTTLE_MS, value: throttleMs });
  }

  for (const name of Object.keys(draft.advanced).sort((left, right) =>
    left.localeCompare(right),
  )) {
    const change = draft.advanced[name];
    if (change === undefined) continue;
    if (change.action === 'reset') {
      resets.push(name);
    } else {
      writes.push({ name, value: change.value });
    }
  }

  return { writes, resets };
}

/** Drops a staged change that would not change the server's value. */
export function normaliseAdvancedChange(
  initialValue: unknown,
  change: AdvancedFlagChange,
): AdvancedFlagChange | null {
  if (change.action === 'reset') {
    return initialValue === undefined ? null : change;
  }
  return jsonValuesEqual(initialValue, change.value) ? null : change;
}

export interface AdvancedFlagRow {
  name: string;
  type: string | undefined;
  description: string | undefined;
  value: unknown;
}

/**
 * Every flag without a typed control: documented flags the deployment did not
 * set yet, and any flag stored on the config app that is not documented.
 */
export function advancedFlagRows(
  known: ReadonlyArray<KnownOperatorFlag>,
  flags: Record<string, unknown>,
): AdvancedFlagRow[] {
  const managed = new Set<string>(MANAGED_FLAG_NAMES);
  const rows = new Map<string, AdvancedFlagRow>();

  for (const flag of known) {
    if (managed.has(flag.name)) continue;
    rows.set(flag.name, {
      name: flag.name,
      type: flag.type,
      description: flag.description,
      value: flags[flag.name],
    });
  }

  for (const [name, value] of Object.entries(flags)) {
    if (managed.has(name) || rows.has(name)) continue;
    rows.set(name, {
      name,
      type: undefined,
      description: undefined,
      value,
    });
  }

  return [...rows.values()].sort((left, right) =>
    left.name.localeCompare(right.name),
  );
}

export function formatFlagValue(value: unknown): string | undefined {
  if (value === undefined) return undefined;
  return JSON.stringify(value);
}

export function isRestoreJobActive(
  job: Pick<RestoreJob, 'job_status'>,
): boolean {
  return job.job_status === 'waiting' || job.job_status === 'processing';
}

export function restoreJobLabel(job: Pick<RestoreJob, 'id' | 'title'>): string {
  const title = job.title?.trim();
  return title !== undefined && title !== ''
    ? title
    : `Restore ${job.id.slice(0, 8)}`;
}

function isString(value: unknown): value is string {
  return typeof value === 'string';
}

function isEmail(value: string): boolean {
  return /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(value);
}

/** IPv4 or IPv6, with an optional prefix; ports and hostnames are rejected. */
export function isIpOrCidr(value: string): boolean {
  const parts = value.split('/');
  const host = parts[0];
  if (host === undefined || parts.length > 2) return false;

  const maxPrefix = ipPrefixLimit(host);
  if (maxPrefix === undefined) return false;

  const prefix = parts[1];
  if (prefix === undefined) return true;
  if (!/^\d{1,3}$/.test(prefix)) return false;
  const length = Number(prefix);
  return length >= 0 && length <= maxPrefix;
}

function ipPrefixLimit(host: string): number | undefined {
  const octets = host.split('.');
  if (
    octets.length === 4 &&
    octets.every(
      (part) =>
        /^\d{1,3}$/.test(part) && Number(part) >= 0 && Number(part) <= 255,
    )
  ) {
    return 32;
  }

  if (!host.includes(':')) return undefined;
  try {
    new URL(`http://[${host}]/`);
    return 128;
  } catch {
    return undefined;
  }
}

function sameList(left: ReadonlyArray<string>, right: ReadonlyArray<string>) {
  return (
    left.length === right.length &&
    left.every((value, index) => value === right[index])
  );
}

/** Key-order independent comparison for JSON flag values. */
export function jsonValuesEqual(left: unknown, right: unknown): boolean {
  return canonicalJson(left) === canonicalJson(right);
}

function canonicalJson(value: unknown): string {
  if (value === undefined) return 'undefined';
  if (Array.isArray(value)) {
    return `[${value.map(canonicalJson).join(',')}]`;
  }
  if (isRecord(value)) {
    return `{${Object.keys(value)
      .sort((left, right) => left.localeCompare(right))
      .map((key) => `${key}:${canonicalJson(value[key])}`)
      .join(',')}}`;
  }
  return JSON.stringify(value);
}
