import {
  PERMISSION_ACTIONS,
  describeEffectiveRule,
  effectiveRule,
  namespaceAllowsPublicWrites,
  type EffectiveRuleView,
  type PermissionAction,
} from './effective';
import { relationshipRows } from './document';

/**
 * C.10 Overview: one row per namespace, with the effective state of every
 * object action and a count of its link rules. The table is derived from the
 * shared draft, so it updates before a save without another server call.
 */

export interface PermissionOverviewCell extends EffectiveRuleView {
  expression: string;
}

export interface PermissionOverviewRow {
  namespace: string;
  isSystem: boolean;
  /** A public write needs attention; user registration is excluded. */
  publicWrites: boolean;
  actions: Record<PermissionAction, PermissionOverviewCell>;
  linkRules: number;
}

export interface PermissionOverviewFilters {
  onlyPublicWrites: boolean;
  hideSystemNamespaces: boolean;
}

export const DEFAULT_OVERVIEW_FILTERS: PermissionOverviewFilters = {
  onlyPublicWrites: false,
  hideSystemNamespaces: false,
};

function cellFor(
  rules: unknown,
  namespace: string,
  action: PermissionAction,
): PermissionOverviewCell {
  const rule = effectiveRule(rules, namespace, action);
  return {
    ...describeEffectiveRule(rule, namespace),
    expression: rule.expression,
  };
}

export function permissionOverviewRows(
  rules: unknown,
  namespaces: readonly string[],
): PermissionOverviewRow[] {
  const seen = new Set<string>();
  const rows: PermissionOverviewRow[] = [];
  for (const namespace of namespaces) {
    if (seen.has(namespace)) continue;
    seen.add(namespace);
    const actions = {} as Record<PermissionAction, PermissionOverviewCell>;
    for (const action of PERMISSION_ACTIONS) {
      actions[action] = cellFor(rules, namespace, action);
    }
    rows.push({
      namespace,
      isSystem: namespace.startsWith('$'),
      publicWrites: namespaceAllowsPublicWrites(rules, namespace),
      actions,
      linkRules: relationshipRows(rules, namespace, 'link').length,
    });
  }
  return rows;
}

export function filterPermissionOverviewRows(
  rows: readonly PermissionOverviewRow[],
  filters: PermissionOverviewFilters,
): PermissionOverviewRow[] {
  return rows.filter(
    (row) =>
      (!filters.onlyPublicWrites || row.publicWrites) &&
      (!filters.hideSystemNamespaces || !row.isSystem),
  );
}
