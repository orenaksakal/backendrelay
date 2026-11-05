import { describe, expect, it } from 'vitest';
import type { AppAttribute, AppLink, AppNamespace } from '@/lib/instant';
import { contextPathAt, instaqlNamespaces, instaqlOptionsAt } from './instaql';

const titleAttribute: AppAttribute = {
  id: 'attr-title',
  namespace: 'todos',
  name: 'title',
  type: 'string',
  isIndexed: false,
  isUnique: false,
  isRequired: false,
  isPrimary: false,
};

const idAttribute: AppAttribute = {
  ...titleAttribute,
  id: 'attr-id',
  name: 'id',
};

const ownerLink: AppLink = {
  id: 'attr-owner',
  namespace: 'todos',
  label: 'owner',
  targetNamespace: '$users',
  reverseLabel: 'todos',
  cardinality: 'many',
  onDelete: undefined,
};

const namespaces: AppNamespace[] = [
  {
    name: 'todos',
    isSystem: false,
    attributes: [idAttribute, titleAttribute],
    links: [ownerLink],
  },
];

const schema = instaqlNamespaces(namespaces);

describe('playground InstaQL completion', () => {
  const document = `{
  "todos": {
    "$": {
      "limit": 10
    },
    "title": ""
  },
  "goals": {}
}`;

  it('finds the object path around the cursor', () => {
    expect(contextPathAt(document, document.indexOf('"todos"'))).toEqual([
      null,
    ]);
    expect(contextPathAt(document, document.indexOf('"title"'))).toEqual([
      null,
      'todos',
    ]);
    expect(contextPathAt(document, document.indexOf('"limit"'))).toEqual([
      null,
      'todos',
      '$',
    ]);
    expect(contextPathAt('{"goals": {}}', 2)).toEqual([null]);
  });

  it('offers namespaces at the root', () => {
    const labels = instaqlOptionsAt(schema, [null]).map(
      (candidate) => candidate.label,
    );
    expect(labels).toEqual(['todos']);
  });

  it('offers attributes, links and $ inside a namespace', () => {
    const labels = instaqlOptionsAt(schema, [null, 'todos']).map(
      (candidate) => candidate.label,
    );
    expect(labels).toContain('$');
    expect(labels).toContain('id');
    expect(labels).toContain('title');
    expect(labels).toContain('owner');
  });

  it('offers the filter keys inside $', () => {
    const labels = instaqlOptionsAt(schema, [null, 'todos', '$']).map(
      (candidate) => candidate.label,
    );
    expect(labels).toEqual(['where', 'order', 'limit', 'fields', 'aggregate']);
  });
});
