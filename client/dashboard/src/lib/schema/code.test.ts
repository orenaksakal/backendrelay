import { describe, expect, it } from 'vitest';
import type { InstantAttr } from '@/lib/instant';
import {
  SCHEMA_TYPESCRIPT_FILENAME,
  platformSchemaFromAttrs,
  schemaTypescriptFile,
} from './code';

const attrs: Record<string, InstantAttr> = {
  'attr-id': {
    id: 'attr-id',
    'forward-identity': ['ident-id', 'todos', 'id', false],
    'index?': false,
    'unique?': true,
    cardinality: 'one',
    'value-type': 'blob',
    'checked-data-type': 'string',
    catalog: 'system',
  },
  'attr-title': {
    id: 'attr-title',
    'forward-identity': ['ident-title', 'todos', 'title'],
    'index?': true,
    'unique?': false,
    'required?': true,
    cardinality: 'one',
    'value-type': 'blob',
    'checked-data-type': 'string',
  },
  'attr-done': {
    id: 'attr-done',
    'forward-identity': ['ident-done', 'todos', 'done'],
    'index?': false,
    'unique?': false,
    'required?': false,
    cardinality: 'one',
    'value-type': 'blob',
    'checked-data-type': 'boolean',
  },
  'attr-owner': {
    id: 'attr-owner',
    'forward-identity': ['ident-owner', 'todos', 'owner'],
    'reverse-identity': ['ident-owner-reverse', '$users', 'todos'],
    'index?': false,
    'unique?': false,
    cardinality: 'one',
    'value-type': 'ref',
    'on-delete': 'cascade',
  },
  'attr-email': {
    id: 'attr-email',
    'forward-identity': ['ident-email', '$users', 'email'],
    'index?': true,
    'unique?': true,
    cardinality: 'one',
    'value-type': 'blob',
    'checked-data-type': 'string',
    catalog: 'system',
  },
  'attr-token': {
    id: 'attr-token',
    'forward-identity': ['ident-token', '$token', 'code'],
    'index?': false,
    'unique?': false,
    cardinality: 'one',
    'value-type': 'blob',
    catalog: 'system',
  },
  'attr-files-size': {
    id: 'attr-files-size',
    'forward-identity': ['ident-files-size', '$files', 'size'],
    'index?': false,
    'unique?': false,
    cardinality: 'one',
    'value-type': 'blob',
    catalog: 'system',
  },
};

describe('platform schema conversion', () => {
  it('groups blobs by namespace and refs by both identities', () => {
    const schema = platformSchemaFromAttrs(attrs);

    expect(Object.keys(schema.blobs).sort()).toEqual(['$users', 'todos']);
    expect(Object.keys(schema.blobs['todos'] ?? {}).sort()).toEqual([
      'done',
      'title',
    ]);
    expect(schema.blobs['$users']).toHaveProperty('email');
    expect(Object.keys(schema.refs)).toEqual([
      JSON.stringify(['todos', 'owner', '$users', 'todos']),
    ]);
  });

  it('hides system catalogs and $files internals', () => {
    const schema = platformSchemaFromAttrs(attrs);
    expect(Object.keys(schema.blobs)).not.toContain('$token');
    expect(schema.blobs['$files']).toBeUndefined();
  });

  it('generates the TypeScript file with entities and links', () => {
    const code = schemaTypescriptFile(attrs);

    expect(code).toContain('i.schema({');
    expect(code).toContain('"todos": i.entity({');
    expect(code).toContain('"title": i.string().indexed()');
    expect(code).toContain('"done": i.boolean().optional()');
    expect(code).toContain('links:');
    expect(code).toContain('"owner"');
    expect(code).toContain('export default schema;');
    expect(SCHEMA_TYPESCRIPT_FILENAME).toBe('instant.schema.ts');
  });

  it('shows a loading comment before the catalog arrives', () => {
    expect(schemaTypescriptFile(null)).toBe('/* Loading schema… */');
    expect(platformSchemaFromAttrs(null)).toEqual({ refs: {}, blobs: {} });
  });

  it('skips attributes whose reverse identity is null', () => {
    const withNullReverse: InstantAttr = {
      id: 'attr-null',
      'forward-identity': ['ident-null', 'todos', 'owner'],
      'reverse-identity': null,
      'index?': false,
      'unique?': false,
      cardinality: 'one',
      'value-type': 'ref',
    };

    expect(() =>
      platformSchemaFromAttrs({ 'attr-null': withNullReverse }),
    ).not.toThrow();
    expect(platformSchemaFromAttrs({ 'attr-null': withNullReverse })).toEqual({
      refs: {},
      blobs: {},
    });
  });
});
