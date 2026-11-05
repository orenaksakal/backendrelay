import { describe, expect, it } from 'vitest';
import type { AppNamespace } from '@/lib/instant';
import {
  columnLabel,
  explorerColumns,
  isHideableColumn,
  visibleColumns,
} from './columns';

const NAMESPACE: AppNamespace = {
  name: 'todos',
  isSystem: false,
  attributes: [
    {
      id: 'attr-id',
      namespace: 'todos',
      name: 'id',
      type: 'string',
      isIndexed: false,
      isUnique: true,
      isRequired: false,
      isPrimary: true,
    },
    {
      id: 'attr-title',
      namespace: 'todos',
      name: 'title',
      type: 'string',
      isIndexed: true,
      isUnique: false,
      isRequired: true,
      isPrimary: false,
    },
  ],
  links: [
    {
      id: 'attr-owner',
      namespace: 'todos',
      label: 'owner',
      targetNamespace: '$users',
      reverseLabel: 'todos',
      cardinality: 'one',
      onDelete: undefined,
    },
  ],
};

describe('explorer columns', () => {
  it('puts attributes (id first) before links', () => {
    const columns = explorerColumns(NAMESPACE);
    expect(columns.map(columnLabel)).toEqual(['id', 'title', 'owner']);
    expect(columns.map((column) => column.id)).toEqual([
      'id',
      'title',
      'link:attr-owner',
    ]);
    expect(explorerColumns(null)).toEqual([]);
  });

  it('never lets the identity column be hidden', () => {
    const [id, title, owner] = explorerColumns(NAMESPACE);
    expect(id).toBeDefined();
    expect(title).toBeDefined();
    expect(owner).toBeDefined();
    if (id === undefined || title === undefined || owner === undefined) return;

    expect(isHideableColumn(id)).toBe(false);
    expect(isHideableColumn(title)).toBe(true);
    expect(isHideableColumn(owner)).toBe(true);

    expect(
      visibleColumns([id, title, owner], ['title']).map(columnLabel),
    ).toEqual(['id', 'owner']);
  });
});
