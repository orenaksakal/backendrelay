import type { App } from '@/lib/api/endpoints/schemas';
import { backupIsStale } from '@/lib/apps/apps-home';
import { formatRelativeTime } from '@/lib/format';
import { appHref } from '@/lib/navigation';
import { publicWriteWarningActions } from '@/lib/permissions/effective';

/**
 * C.5 overview model: the KPI sparkline, the live-session origins table and the
 * health checks. The permission logic mirrors the server's inheritance order
 * (C.10) so the overview warning agrees with the Permissions screen.
 */

export const STATS_POLL_INTERVAL_MS = 5_000;

/** Polling stops while the document is hidden, then resumes on return. */
export function statsPollInterval(documentHidden: boolean): number | false {
  return documentHidden ? false : STATS_POLL_INTERVAL_MS;
}

/** React Query reads `document.hidden` every time it schedules a refetch. */
export function statsRefetchInterval(): number | false {
  return statsPollInterval(typeof document !== 'undefined' && document.hidden);
}

export interface DailyWrites {
  date: string;
  count: number;
}

/** The KPI sparkline covers the last seven days of the D0.2 daily series. */
export function lastSevenDays(
  daily: ReadonlyArray<DailyWrites>,
): DailyWrites[] {
  return daily.slice(-7);
}

export interface SessionOriginRow {
  origin: string;
  sessions: number;
}

/** Highest-traffic origins first, matching the old dashboard's activity list. */
export function sessionOriginRows(
  stats: { origins: Record<string, number> } | undefined,
): SessionOriginRow[] {
  return Object.entries(stats?.origins ?? {})
    .map(([origin, sessions]) => ({ origin, sessions }))
    .sort((left, right) => right.sessions - left.sessions);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function allowValue(block: unknown, action: string): unknown {
  if (!isRecord(block)) return undefined;
  const allow = block['allow'];
  if (allow === undefined) return undefined;
  // Older documents may store a single expression for every action.
  if (typeof allow === 'string' || typeof allow === 'boolean') return allow;
  if (!isRecord(allow)) return undefined;
  return allow[action];
}

/**
 * Effective rule for one namespace action, following the server's order:
 * namespace action, namespace `$default`, app `$default` action, app
 * `$default.$default`, then the namespace kind's default (system denies, user
 * namespaces allow).
 */
export function effectiveAllowRule(
  rules: unknown,
  namespace: string,
  action: string,
): unknown {
  const document = isRecord(rules) ? rules : {};
  const namespaceBlock = document[namespace];

  const own = allowValue(namespaceBlock, action);
  if (own !== undefined && own !== '$default') return own;

  const namespaceDefault = allowValue(namespaceBlock, '$default');
  if (namespaceDefault !== undefined) return namespaceDefault;

  const appDefault = document['$default'];
  const appAction = allowValue(appDefault, action);
  if (appAction !== undefined) return appAction;

  const appDefaultRule = allowValue(appDefault, '$default');
  if (appDefaultRule !== undefined) return appDefaultRule;

  return namespace.startsWith('$') ? false : true;
}

/** Only an explicit `true` means everyone; CEL strings are not evaluated here. */
export function ruleAllowsEveryone(rule: unknown): boolean {
  return (
    rule === true ||
    (typeof rule === 'string' && rule.trim().toLowerCase() === 'true')
  );
}

/** Namespaces with public writes that need attention (not user registration). */
export function publicWriteNamespaces(rules: unknown): string[] {
  if (!isRecord(rules)) return [];
  return Object.keys(rules)
    .filter((name) => name !== '$default' && name !== '$rateLimits')
    .filter((name) =>
      publicWriteWarningActions(name).some((action) =>
        ruleAllowsEveryone(effectiveAllowRule(rules, name, action)),
      ),
    );
}

export function appHasCustomSender(
  app: Pick<App, 'magic_code_email_template'>,
): boolean {
  const email = app.magic_code_email_template?.email;
  return typeof email === 'string' && email.trim() !== '';
}

export type HealthTone = 'ok' | 'warning' | 'pending';

export interface OverviewHealthCheck {
  id: 'permissions' | 'backups' | 'webhooks' | 'auth';
  tone: HealthTone;
  text: string;
  detail?: string;
  href: string;
  actionLabel: string;
}

export interface OverviewHealthInput {
  appId: string;
  rules: unknown;
  webhooks: ReadonlyArray<{ status: 'active' | 'disabled' }>;
  canAdmin: boolean;
  hasCustomSender: boolean;
  senderVerified: boolean | undefined;
  senderLoaded: boolean;
  lastBackupAt: string | null;
  backupsLoaded: boolean;
  backupsFailed: boolean;
  hasBackupSchedule: boolean;
  scheduleLoaded: boolean;
  now?: Date;
}

function permissionsCheck(input: OverviewHealthInput): OverviewHealthCheck {
  const configured =
    isRecord(input.rules) &&
    Object.keys(input.rules).some((name) => name !== '$rateLimits');
  if (!configured) {
    return {
      id: 'permissions',
      tone: 'warning',
      text: 'Permissions: no rules configured; user namespaces allow public writes',
      detail: 'Add rules before accepting writes from untrusted clients.',
      href: `${appHref(input.appId, 'permissions')}?view=overview`,
      actionLabel: 'Open permissions',
    };
  }

  const appDefault = isRecord(input.rules)
    ? input.rules['$default']
    : undefined;
  const appDefaultPublic =
    publicWriteWarningActions('$default').some((action) =>
      ruleAllowsEveryone(allowValue(appDefault, action)),
    ) || ruleAllowsEveryone(allowValue(appDefault, '$default'));
  const namespaces = publicWriteNamespaces(input.rules);
  const count = namespaces.length;
  const text = appDefaultPublic
    ? 'Permissions: the app default allows public writes'
    : count === 0
      ? 'Permissions: no unexpected public writes'
      : `Permissions: ${String(count)} ${count === 1 ? 'namespace allows' : 'namespaces allow'} public writes`;
  const shown = namespaces.slice(0, 5).join(', ');
  return {
    id: 'permissions',
    tone: appDefaultPublic || count > 0 ? 'warning' : 'ok',
    text,
    detail: appDefaultPublic
      ? 'The app-level $default rule allows writes from everyone.'
      : count > 0
        ? `Namespaces: ${shown}${count > 5 ? ', …' : ''}`
        : undefined,
    href: `${appHref(input.appId, 'permissions')}?view=overview`,
    actionLabel: 'Open permissions',
  };
}

function backupsCheck(input: OverviewHealthInput): OverviewHealthCheck {
  const href = appHref(input.appId, 'backups');
  if (input.backupsFailed) {
    return {
      id: 'backups',
      tone: 'warning',
      text: 'Backups: last backup unavailable',
      href,
      actionLabel: 'Open backups',
    };
  }
  if (!input.backupsLoaded || !input.scheduleLoaded) {
    return {
      id: 'backups',
      tone: 'pending',
      text: 'Backups: checking…',
      href,
      actionLabel: 'Open backups',
    };
  }

  const last =
    input.lastBackupAt === null
      ? 'no backups yet'
      : `last backup ${formatRelativeTime(input.lastBackupAt, input.now)}`;
  const schedule = input.hasBackupSchedule ? 'scheduled' : 'no schedule';
  const stale =
    input.lastBackupAt === null ||
    backupIsStale(input.lastBackupAt, input.now?.getTime());

  return {
    id: 'backups',
    tone: stale || !input.hasBackupSchedule ? 'warning' : 'ok',
    text: `Backups: ${last} · ${schedule}`,
    detail:
      input.lastBackupAt === null
        ? undefined
        : stale
          ? 'No backup in the last 7 days.'
          : undefined,
    href,
    actionLabel: 'Open backups',
  };
}

function webhooksCheck(input: OverviewHealthInput): OverviewHealthCheck {
  const disabled = input.webhooks.filter(
    (webhook) => webhook.status === 'disabled',
  ).length;
  const text =
    disabled > 0
      ? `Webhooks: ${String(disabled)} disabled`
      : input.webhooks.length === 0
        ? 'Webhooks: none configured'
        : 'Webhooks: all active';
  return {
    id: 'webhooks',
    tone: disabled > 0 ? 'warning' : 'ok',
    text,
    href: appHref(input.appId, 'webhooks'),
    actionLabel: 'Open webhooks',
  };
}

function authCheck(input: OverviewHealthInput): OverviewHealthCheck {
  const href = `${appHref(input.appId, 'auth')}?tab=email`;
  if (!input.canAdmin) {
    return {
      id: 'auth',
      tone: 'ok',
      text: 'Auth: sender verification needs admin access',
      href,
      actionLabel: 'Open auth',
    };
  }
  if (!input.senderLoaded) {
    return {
      id: 'auth',
      tone: 'pending',
      text: 'Auth: checking sender verification…',
      href,
      actionLabel: 'Open auth',
    };
  }
  if (input.hasCustomSender && input.senderVerified !== true) {
    return {
      id: 'auth',
      tone: 'warning',
      text: 'Auth: custom sender domain not verified (using the default BackendRelay sender)',
      href,
      actionLabel: 'Open auth',
    };
  }
  return {
    id: 'auth',
    tone: 'ok',
    text: input.hasCustomSender
      ? 'Auth: custom sender domain verified'
      : 'Auth: using the default BackendRelay sender',
    href,
    actionLabel: 'Open auth',
  };
}

/** The four C.5 health rows, in the order the screen lists them. */
export function buildOverviewHealth(
  input: OverviewHealthInput,
): OverviewHealthCheck[] {
  return [
    permissionsCheck(input),
    backupsCheck(input),
    webhooksCheck(input),
    authCheck(input),
  ];
}
