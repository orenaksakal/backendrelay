import { describe, expect, it } from 'vitest';
import {
  DEFAULT_OVERVIEW_FILTERS,
  filterPermissionOverviewRows,
  permissionOverviewRows,
} from './overview';

const rules = {
  todos: {
    allow: {
      view: 'true',
      create: 'auth.id != null',
      update: 'false',
      delete: 'false',
      link: { owner: 'auth.id == ownerId' },
    },
  },
  goals: { allow: { create: 'true' } },
};

describe('permissions overview', () => {
  it('derives one row per namespace with every effective action', () => {
    const rows = permissionOverviewRows(rules, ['$default', 'todos', 'goals']);
    expect(rows.map((row) => row.namespace)).toEqual([
      '$default',
      'todos',
      'goals',
    ]);

    const todos = rows.find((row) => row.namespace === 'todos');
    expect(todos).toBeDefined();
    expect(todos?.actions.view).toMatchObject({
      state: 'everyone',
      expression: 'true',
    });
    expect(todos?.actions.update).toMatchObject({
      state: 'admin-only',
      expression: 'false',
    });
    expect(todos?.actions.create).toMatchObject({ state: 'custom' });
    expect(todos?.linkRules).toBe(1);
    expect(todos?.publicWrites).toBe(false);
  });

  it('warns about namespaces whose writes allow everyone', () => {
    const rows = permissionOverviewRows(rules, ['goals', 'notes']);
    const goals = rows.find((row) => row.namespace === 'goals');
    expect(goals?.publicWrites).toBe(true);
    // A namespace with no rule at all is the implicit public-write case.
    const notes = rows.find((row) => row.namespace === 'notes');
    expect(notes?.actions.delete).toMatchObject({ state: 'no-rule' });
    expect(notes?.publicWrites).toBe(true);
  });

  it('shows public registration without flagging $users as unsafe', () => {
    const rows = permissionOverviewRows(
      { $users: { allow: { create: 'true' } } },
      ['$users'],
    );
    expect(rows[0]?.actions.create).toMatchObject({
      state: 'everyone',
      expression: 'true',
    });
    expect(rows[0]?.publicWrites).toBe(false);
  });

  it('filters public writes and system namespaces independently', () => {
    const rows = permissionOverviewRows(rules, [
      '$default',
      '$users',
      'todos',
      'goals',
    ]);

    expect(
      filterPermissionOverviewRows(rows, {
        ...DEFAULT_OVERVIEW_FILTERS,
        onlyPublicWrites: true,
      }).map((row) => row.namespace),
    ).toEqual(['goals']);

    expect(
      filterPermissionOverviewRows(rows, {
        ...DEFAULT_OVERVIEW_FILTERS,
        hideSystemNamespaces: true,
        onlyPublicWrites: true,
      }).map((row) => row.namespace),
    ).toEqual(['goals']);
  });

  it('does not repeat a namespace that appears twice in the schema', () => {
    const rows = permissionOverviewRows(rules, ['todos', 'todos']);
    expect(rows).toHaveLength(1);
  });
});
