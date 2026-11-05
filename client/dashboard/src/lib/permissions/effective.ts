/**
 * C.10/D7.2: the effective rule for one namespace action.
 *
 * The server (`RuleSet::program`) looks up, in order: the namespace action, the
 * namespace `$default`, the app `$default` action and the app `$default`
 * `$default`. When none of those exists a user namespace has no program, which
 * the runtime treats as allowed; system namespaces fall back to their built-in
 * rules. `server-rs/crates/instant-server/tests/live_permissions_default.rs`
 * pins the user-namespace half of that contract.
 */

export const PERMISSION_ACTIONS = [
  'view',
  'create',
  'update',
  'delete',
] as const;

export type PermissionAction = (typeof PERMISSION_ACTIONS)[number];

export const PERMISSION_ACTION_LABELS: Record<PermissionAction, string> = {
  view: 'View',
  create: 'Create',
  update: 'Update',
  delete: 'Delete',
};

/** Where the effective expression came from. */
export type EffectiveSource =
  | 'own'
  | 'namespace-default'
  | 'app-action'
  | 'app-default'
  | 'system-default';

export interface EffectiveRule {
  /** The CEL source; booleans are normalized to `"true"`/`"false"`. */
  expression: string;
  source: EffectiveSource;
  /** True when the namespace has its own rule for this action. */
  own: boolean;
}

export type EffectiveState =
  | 'everyone'
  | 'admin-only'
  | 'inherited'
  | 'no-rule'
  | 'custom'
  | 'system-default';

export interface EffectiveRuleView {
  state: EffectiveState;
  label: string;
  tone: 'neutral' | 'accent' | 'success' | 'warning' | 'destructive';
  /** The expression and where it comes from, for the chip's description. */
  description: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

/** `undefined` means "no expression here"; `null` is treated the same way. */
function actionEntry(
  allow: unknown,
  action: PermissionAction | '$default',
): string | undefined {
  // Older documents may store one expression for every action.
  if (typeof allow === 'string') return allow;
  if (typeof allow === 'boolean') return String(allow);
  if (!isRecord(allow)) return undefined;
  const value = allow[action];
  if (typeof value === 'string') return value;
  if (typeof value === 'boolean') return String(value);
  return undefined;
}

/**
 * The expression the server compiles for a system namespace when the document
 * has no rule. `$users` is the one namespace with a non-boolean fallback; other
 * system namespaces deny everything (see `system_fallback` in
 * `instant-permissions`).
 */
export function systemDefaultExpression(
  namespace: string,
  action: PermissionAction,
): string {
  if (!namespace.startsWith('$')) return 'true';
  if (namespace !== '$users') return 'false';
  switch (action) {
    case 'create':
      return 'true';
    case 'view':
    case 'update':
      return 'auth.id == data.id || (data.linkedPrimaryUser != null && auth.id == data.linkedPrimaryUser)';
    default:
      return 'false';
  }
}

export function effectiveRule(
  rules: unknown,
  namespace: string,
  action: PermissionAction,
): EffectiveRule {
  const document = isRecord(rules) ? rules : {};
  const block = document[namespace];
  const allow = isRecord(block) ? block['allow'] : undefined;

  const own = actionEntry(allow, action);
  if (own !== undefined) {
    return { expression: own, source: 'own', own: true };
  }

  const namespaceDefault = actionEntry(allow, '$default');
  if (namespaceDefault !== undefined) {
    return {
      expression: namespaceDefault,
      source: 'namespace-default',
      own: false,
    };
  }

  const appDefault = isRecord(document['$default'])
    ? document['$default']['allow']
    : undefined;
  const appAction = actionEntry(appDefault, action);
  if (appAction !== undefined) {
    return { expression: appAction, source: 'app-action', own: false };
  }

  const appDefaultRule = actionEntry(appDefault, '$default');
  if (appDefaultRule !== undefined) {
    return { expression: appDefaultRule, source: 'app-default', own: false };
  }

  return {
    expression: systemDefaultExpression(namespace, action),
    source: 'system-default',
    own: false,
  };
}

export function allowsEveryone(expression: string): boolean {
  return expression.trim().toLowerCase() === 'true';
}

export function deniesEveryone(expression: string): boolean {
  return expression.trim().toLowerCase() === 'false';
}

const SOURCE_DESCRIPTIONS: Record<EffectiveSource, string> = {
  own: 'this action',
  'namespace-default': `the namespace's allow.$default`,
  'app-action': 'the app $default rule for this action',
  'app-default': 'the app $default.allow.$default rule',
  'system-default': "the server's built-in rule",
};

/** Chip text and tone for the effective rule (C.10). */
export function describeEffectiveRule(
  rule: EffectiveRule,
  namespace: string,
): EffectiveRuleView {
  const description = `${rule.expression} (from ${SOURCE_DESCRIPTIONS[rule.source]})`;

  if (rule.source === 'system-default' && !namespace.startsWith('$')) {
    return {
      state: 'no-rule',
      label: 'No rule — allows everyone',
      tone: 'warning',
      description: `${rule.expression} (the server allows a user namespace without a rule)`,
    };
  }

  if (rule.source !== 'own' && rule.source !== 'system-default') {
    return {
      state: 'inherited',
      label: 'Inherited · $default',
      tone: 'neutral',
      description,
    };
  }

  if (deniesEveryone(rule.expression)) {
    return {
      state: 'admin-only',
      label: 'Admin only',
      tone: 'neutral',
      description,
    };
  }
  if (allowsEveryone(rule.expression)) {
    return {
      state: 'everyone',
      label: 'Everyone',
      tone: 'neutral',
      description,
    };
  }
  if (rule.source === 'system-default') {
    return {
      state: 'system-default',
      label: 'Server default',
      tone: 'neutral',
      description,
    };
  }
  return {
    state: 'custom',
    label: 'Custom rule',
    tone: 'accent',
    description,
  };
}

/**
 * Public writes that warrant a warning. `$users.create` is public by design
 * so visitors can register before they have an authenticated account.
 */
export function publicWriteWarningActions(
  namespace: string,
): ReadonlyArray<PermissionAction> {
  return namespace === '$users'
    ? ['update', 'delete']
    : ['create', 'update', 'delete'];
}

/** True when a public write needs attention, excluding user registration. */
export function namespaceAllowsPublicWrites(
  rules: unknown,
  namespace: string,
): boolean {
  return publicWriteWarningActions(namespace).some((action) =>
    allowsEveryone(effectiveRule(rules, namespace, action).expression),
  );
}
