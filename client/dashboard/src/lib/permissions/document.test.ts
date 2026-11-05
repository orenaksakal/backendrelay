import { describe, expect, it } from 'vitest';
import {
  actionRowKey,
  addNamespace,
  applyTemplate,
  bindRowKey,
  editorNamespaces,
  fieldRows,
  namespaceUnsupportedReason,
  ownExpression,
  readBinds,
  relationshipRows,
  setAllowExpression,
  setBinds,
  setFieldExpression,
  setRelationshipExpression,
  templateAllow,
} from './document';

const base = {
  todos: {
    allow: {
      view: 'true',
      create: 'auth.id != null',
      $default: 'false',
    },
    fields: { title: 'data.done == true' },
    bind: { owner: 'auth.id' },
  },
  $default: { allow: { update: 'false' } },
};

describe('permission document editing', () => {
  it('sets and removes one action without touching the rest', () => {
    const updated = setAllowExpression(base, 'todos', 'view', 'false');
    expect(updated['todos']).toEqual({
      allow: {
        view: 'false',
        create: 'auth.id != null',
        $default: 'false',
      },
      fields: { title: 'data.done == true' },
      bind: { owner: 'auth.id' },
    });
    expect(base['todos']['allow']).toMatchObject({ view: 'true' });

    const reset = setAllowExpression(updated, 'todos', 'view', null);
    expect(ownExpression(reset, 'todos', 'view')).toBeNull();
    const resetTodos = reset['todos'] as Record<string, unknown> | undefined;
    expect(resetTodos?.['allow']).toEqual({
      create: 'auth.id != null',
      $default: 'false',
    });
  });

  it('drops empty containers when the last rule is reset', () => {
    const reset = setAllowExpression(
      { todos: { allow: { view: 'true' } } },
      'todos',
      'view',
      null,
    );
    expect(reset).toEqual({});
  });

  it('keeps bind and fields out of the action reset', () => {
    const reset = setAllowExpression(
      { todos: { allow: { view: 'true' }, fields: { title: 'true' } } },
      'todos',
      'view',
      null,
    );
    expect(reset).toEqual({ todos: { fields: { title: 'true' } } });
  });

  it('reads and writes link/unlink and field expressions', () => {
    const withRelationship = setRelationshipExpression(
      base,
      'todos',
      'link',
      'owner',
      "auth.id == data.ref('owner.id')",
    );
    expect(relationshipRows(withRelationship, 'todos', 'link')).toEqual([
      { name: 'owner', expression: "auth.id == data.ref('owner.id')" },
    ]);
    expect(relationshipRows(withRelationship, 'todos', 'unlink')).toEqual([]);

    const withoutRelationship = setRelationshipExpression(
      withRelationship,
      'todos',
      'link',
      'owner',
      null,
    );
    expect(relationshipRows(withoutRelationship, 'todos', 'link')).toEqual([]);

    const withField = setFieldExpression(base, 'todos', 'done', 'true');
    expect(fieldRows(withField, 'todos')).toEqual([
      { name: 'done', expression: 'true' },
      { name: 'title', expression: 'data.done == true' },
    ]);
    expect(
      fieldRows(setFieldExpression(withField, 'todos', 'done', null), 'todos'),
    ).toEqual([{ name: 'title', expression: 'data.done == true' }]);
  });

  it('preserves both bind shapes and writes object bindings for new rows', () => {
    expect(readBinds(base, 'todos')).toEqual({
      shape: 'object',
      rows: [{ name: 'owner', expression: 'auth.id' }],
      valid: true,
    });

    const arrayRules = {
      todos: { bind: ['owner', 'auth.id', 'member', 'auth.id != null'] },
    };
    expect(readBinds(arrayRules, 'todos')).toEqual({
      shape: 'array',
      rows: [
        { name: 'owner', expression: 'auth.id' },
        { name: 'member', expression: 'auth.id != null' },
      ],
      valid: true,
    });

    expect(
      setBinds(
        arrayRules,
        'todos',
        [{ name: 'owner', expression: 'false' }],
        readBinds(arrayRules, 'todos').shape,
      ),
    ).toEqual({ todos: { bind: ['owner', 'false'] } });

    expect(
      setBinds(
        base,
        'todos',
        [{ name: 'owner', expression: 'false' }],
        readBinds(base, 'todos').shape,
      ),
    ).toEqual({
      $default: { allow: { update: 'false' } },
      todos: {
        allow: {
          view: 'true',
          create: 'auth.id != null',
          $default: 'false',
        },
        fields: { title: 'data.done == true' },
        bind: { owner: 'false' },
      },
    });

    expect(
      setBinds(
        { todos: { bind: { owner: 'auth.id' } } },
        'todos',
        [],
        'object',
      ),
    ).toEqual({});
  });

  it('rejects bind shapes the editor cannot represent', () => {
    expect(readBinds({ todos: { bind: ['owner'] } }, 'todos')).toMatchObject({
      valid: false,
    });
    expect(readBinds({ todos: { bind: { owner: 7 } } }, 'todos')).toMatchObject(
      { valid: false },
    );
    expect(
      namespaceUnsupportedReason({ todos: { bind: ['owner'] } }, 'todos'),
    ).toContain('bind');
  });

  it('marks keys the visual editor cannot represent', () => {
    expect(namespaceUnsupportedReason(base, 'todos')).toBeNull();
    expect(
      namespaceUnsupportedReason(
        { todos: { allow: { write: 'true' } } },
        'todos',
      ),
    ).toBe('Unknown action "write".');
    expect(namespaceUnsupportedReason({ todos: { extra: {} } }, 'todos')).toBe(
      'Unknown setting "extra".',
    );
    expect(
      namespaceUnsupportedReason({ todos: { allow: 'true' } }, 'todos'),
    ).toBe('allow must map actions to expressions.');
    expect(
      namespaceUnsupportedReason({ todos: { fields: { title: 1 } } }, 'todos'),
    ).toBe('fields must map attributes to expressions.');
  });

  it('lists $default first, then user namespaces, then system namespaces', () => {
    expect(
      editorNamespaces(
        { todos: {}, $default: {}, $rateLimits: {}, attrs: {}, goals: {} },
        ['$users', '$files', 'todos'],
      ),
    ).toEqual(['$default', 'goals', 'todos', '$files', '$users']);
  });

  it('adds an empty namespace for the sidebar', () => {
    expect(addNamespace({}, 'notes')).toEqual({ notes: {} });
    expect(
      addNamespace({ notes: { allow: { view: 'true' } } }, 'notes'),
    ).toEqual({ notes: { allow: { view: 'true' } } });
  });

  it('applies templates to allow only and keeps advanced rules', () => {
    expect(templateAllow('public-read-owner-write')).toEqual({
      view: 'true',
      create: "auth.id in newData.ref('owner.id')",
      update: "auth.id in data.ref('owner.id')",
      delete: "auth.id in data.ref('owner.id')",
    });
    expect(applyTemplate(base, 'todos', 'admin-only')).toEqual({
      todos: {
        allow: {
          view: 'false',
          create: 'false',
          update: 'false',
          delete: 'false',
        },
        fields: { title: 'data.done == true' },
        bind: { owner: 'auth.id' },
      },
      $default: { allow: { update: 'false' } },
    });
    expect(applyTemplate({}, 'notes', 'read-only')).toEqual({
      notes: {
        allow: {
          view: 'true',
          create: 'false',
          update: 'false',
          delete: 'false',
        },
      },
    });
  });

  it('builds the same row keys the server error paths use', () => {
    expect(actionRowKey('todos', 'view')).toBe('todos|allow|view');
    expect(bindRowKey('todos', 'owner')).toBe('todos|bind|owner');
  });
});
