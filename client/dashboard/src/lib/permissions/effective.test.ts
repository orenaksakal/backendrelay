import { describe, expect, it } from 'vitest';
import {
  allowsEveryone,
  describeEffectiveRule,
  effectiveRule,
  namespaceAllowsPublicWrites,
  systemDefaultExpression,
} from './effective';

describe('effective permission rules', () => {
  it('uses the namespace action first', () => {
    const rules = {
      todos: { allow: { create: 'auth.id != null', $default: 'false' } },
      $default: { allow: { create: 'true' } },
    };
    expect(effectiveRule(rules, 'todos', 'create')).toEqual({
      expression: 'auth.id != null',
      source: 'own',
      own: true,
    });
  });

  it("uses the namespace's allow.$default second", () => {
    const rules = {
      todos: { allow: { $default: 'false' } },
      $default: { allow: { create: 'true', $default: 'true' } },
    };
    expect(effectiveRule(rules, 'todos', 'create')).toEqual({
      expression: 'false',
      source: 'namespace-default',
      own: false,
    });
  });

  it('uses $default.allow.<action> third', () => {
    const rules = {
      todos: { allow: {} },
      $default: { allow: { create: 'auth.id != null', $default: 'true' } },
    };
    expect(effectiveRule(rules, 'todos', 'create')).toEqual({
      expression: 'auth.id != null',
      source: 'app-action',
      own: false,
    });
  });

  it('uses $default.allow.$default fourth', () => {
    const rules = {
      todos: {},
      $default: { allow: { $default: 'false' } },
    };
    expect(effectiveRule(rules, 'todos', 'delete')).toEqual({
      expression: 'false',
      source: 'app-default',
      own: false,
    });
  });

  it('allows a user namespace with no rule anywhere', () => {
    const empty = effectiveRule({}, 'todos', 'view');
    expect(empty).toEqual({
      expression: 'true',
      source: 'system-default',
      own: false,
    });
    expect(describeEffectiveRule(empty, 'todos')).toMatchObject({
      state: 'no-rule',
      label: 'No rule — allows everyone',
      tone: 'warning',
    });
  });

  it('keeps the built-in system fallbacks visible', () => {
    expect(effectiveRule({}, '$files', 'view')).toEqual({
      expression: 'false',
      source: 'system-default',
      own: false,
    });
    expect(
      describeEffectiveRule(effectiveRule({}, '$files', 'view'), '$files')
        .state,
    ).toBe('admin-only');

    const usersCreate = effectiveRule({}, '$users', 'create');
    expect(usersCreate.expression).toBe('true');
    expect(describeEffectiveRule(usersCreate, '$users').state).toBe('everyone');
    expect(systemDefaultExpression('$users', 'view')).toContain(
      'linkedPrimaryUser',
    );
  });

  it('labels inherited, locked, public and custom rules', () => {
    expect(
      describeEffectiveRule(effectiveRule({}, 'todos', 'view'), 'todos'),
    ).toMatchObject({ state: 'no-rule' });

    const inherited = effectiveRule(
      { todos: {}, $default: { allow: { $default: 'false' } } },
      'todos',
      'update',
    );
    expect(describeEffectiveRule(inherited, 'todos')).toMatchObject({
      state: 'inherited',
      label: 'Inherited · $default',
      description: 'false (from the app $default.allow.$default rule)',
    });

    const custom = effectiveRule(
      { todos: { allow: { view: 'auth.id in data.ref("owner.id")' } } },
      'todos',
      'view',
    );
    expect(describeEffectiveRule(custom, 'todos')).toMatchObject({
      state: 'custom',
      label: 'Custom rule',
      tone: 'accent',
    });
  });

  it('normalizes older single-expression allow blocks', () => {
    expect(
      effectiveRule({ todos: { allow: 'false' } }, 'todos', 'view'),
    ).toEqual({ expression: 'false', source: 'own', own: true });
    expect(
      effectiveRule({ todos: { allow: true } }, 'todos', 'delete'),
    ).toEqual({ expression: 'true', source: 'own', own: true });
  });

  it('warns about public writes, including the implicit no-rule case', () => {
    expect(allowsEveryone(' TRUE ')).toBe(true);
    expect(allowsEveryone('auth.id != null')).toBe(false);
    expect(
      namespaceAllowsPublicWrites(
        { todos: { allow: { create: 'auth.id != null' } } },
        'todos',
      ),
    ).toBe(true);
    expect(namespaceAllowsPublicWrites({}, 'todos')).toBe(true);
    expect(namespaceAllowsPublicWrites({}, '$files')).toBe(false);
  });

  it.each([
    {},
    { $users: { allow: { create: 'true' } } },
    { $users: { allow: { create: true } } },
    { $default: { allow: { create: 'true' } } },
  ])('does not warn about public user registration: %j', (rules) => {
    expect(effectiveRule(rules, '$users', 'create').expression).toBe('true');
    expect(namespaceAllowsPublicWrites(rules, '$users')).toBe(false);
  });

  it.each(['update', 'delete'] as const)(
    'still warns about public $users %s, including inherited rules',
    (action) => {
      expect(
        namespaceAllowsPublicWrites(
          { $users: { allow: { create: 'true', [action]: 'true' } } },
          '$users',
        ),
      ).toBe(true);
      expect(
        namespaceAllowsPublicWrites(
          { $default: { allow: { [action]: true } } },
          '$users',
        ),
      ).toBe(true);
    },
  );

  it('does not exempt an entire public $users allow block', () => {
    expect(
      namespaceAllowsPublicWrites({ $users: { allow: 'true' } }, '$users'),
    ).toBe(true);
    expect(
      namespaceAllowsPublicWrites(
        { $users: { allow: { $default: 'true' } } },
        '$users',
      ),
    ).toBe(true);
  });
});
