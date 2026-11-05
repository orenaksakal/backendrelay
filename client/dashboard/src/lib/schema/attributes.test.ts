import { describe, expect, it } from 'vitest';
import type { SoftDeletedAttribute } from '@/lib/api/endpoints/apps';
import type {
  AppAttribute,
  AppLink,
  AppNamespace,
  InstantAttr,
} from '@/lib/instant';
import {
  addAttributeOps,
  addLinkOps,
  attributeNameProblem,
  deleteNamespaceOps,
  draftFromAttribute,
  linkCardinalities,
  linkCascades,
  linkConstraints,
  linkDraftProblem,
  pendingConstraintJobs,
  renameAttributeOps,
  restoreAttributeOps,
  softDeletedAttributeView,
  type LinkDraft,
} from './attributes';

const title: AppAttribute = {
  id: 'attr-title',
  namespace: 'todos',
  name: 'title',
  type: 'string',
  isIndexed: true,
  isUnique: false,
  isRequired: true,
  isPrimary: false,
};

const rawTitle: InstantAttr = {
  id: 'attr-title',
  'forward-identity': ['ident-title', 'todos', 'title'],
  'index?': true,
  'unique?': false,
  'required?': true,
  cardinality: 'one',
  'value-type': 'blob',
  'checked-data-type': 'string',
};

const namespace: AppNamespace = {
  name: 'todos',
  isSystem: false,
  attributes: [title],
  links: [],
};

describe('attribute validation', () => {
  it('accepts plain names and rejects the reserved shapes', () => {
    expect(attributeNameProblem('dueAt', ['title'])).toBeUndefined();
    expect(attributeNameProblem('', [])).toBe('Enter a name.');
    expect(attributeNameProblem('$users', [])).toBe(
      'Names cannot start with $.',
    );
    expect(attributeNameProblem('two words', [])).toBe(
      'Use only letters, numbers, _ and -.',
    );
    expect(attributeNameProblem('title', ['title'])).toBe(
      'That attribute already exists.',
    );
    expect(attributeNameProblem('title', ['title'], 'title')).toBeUndefined();
    expect(attributeNameProblem('id', [])).toBe(
      'id is added automatically and cannot be renamed.',
    );
  });
});

describe('attribute transactions', () => {
  it('adds one attribute with its constraints and enforced type', () => {
    const ids = ['attr-new', 'ident-new'];
    const ops = addAttributeOps(
      'todos',
      {
        name: 'dueAt',
        type: 'date',
        required: true,
        unique: false,
        indexed: true,
      },
      () => ids.shift() ?? 'extra',
    );

    expect(ops).toEqual([
      [
        'add-attr',
        {
          id: 'attr-new',
          'forward-identity': ['ident-new', 'todos', 'dueAt'],
          'value-type': 'blob',
          cardinality: 'one',
          'unique?': false,
          'index?': true,
          'required?': true,
          'checked-data-type': 'date',
        },
      ],
    ]);
  });

  it('leaves the type unchecked for json and any', () => {
    const [op] = addAttributeOps(
      'todos',
      {
        name: 'payload',
        type: 'json',
        required: false,
        unique: false,
        indexed: false,
      },
      () => 'id',
    );
    expect(op?.[1]).not.toHaveProperty('checked-data-type');
  });

  it('renames with the existing forward identity id', () => {
    expect(renameAttributeOps(title, 'heading', rawTitle)).toEqual([
      [
        'update-attr',
        {
          id: 'attr-title',
          'forward-identity': ['ident-title', 'todos', 'heading'],
        },
      ],
    ]);
    expect(renameAttributeOps(title, 'title', rawTitle)).toEqual([]);
  });

  it('does not rename without the raw catalog entry', () => {
    expect(renameAttributeOps(title, 'heading', undefined)).toEqual([]);
  });

  it('deletes every attribute and link in a namespace once', () => {
    const link: AppLink = {
      id: 'attr-owner',
      namespace: 'todos',
      label: 'owner',
      targetNamespace: '$users',
      reverseLabel: 'todos',
      cardinality: 'one',
      onDelete: undefined,
    };
    expect(
      deleteNamespaceOps({
        ...namespace,
        links: [link, { ...link, label: 'author' }],
      }),
    ).toEqual([
      ['delete-attr', 'attr-title'],
      ['delete-attr', 'attr-owner'],
    ]);
  });

  it('restores a soft-deleted attribute', () => {
    expect(restoreAttributeOps('attr-title')).toEqual([
      ['restore-attr', 'attr-title'],
    ]);
  });
});

describe('constraint jobs', () => {
  const before = draftFromAttribute(title);

  it('starts a job for each changed constraint', () => {
    expect(
      pendingConstraintJobs(before, {
        ...before,
        required: false,
        indexed: false,
        unique: true,
        type: 'number',
      }),
    ).toEqual([
      { jobType: 'remove-required' },
      { jobType: 'remove-index' },
      { jobType: 'unique' },
      { jobType: 'check-data-type', checkedDataType: 'number' },
    ]);
  });

  it('removes the type check when the type becomes unenforced', () => {
    expect(pendingConstraintJobs(before, { ...before, type: 'any' })).toEqual([
      { jobType: 'remove-data-type' },
    ]);
  });

  it('does nothing for an unchanged or new attribute', () => {
    expect(pendingConstraintJobs(before, before)).toEqual([]);
    expect(
      pendingConstraintJobs(null, {
        name: 'new',
        type: 'string',
        required: true,
        unique: true,
        indexed: true,
      }),
    ).toEqual([]);
  });
});

describe('links', () => {
  const users: AppNamespace = {
    name: '$users',
    isSystem: true,
    attributes: [],
    links: [],
  };

  function draft(patch: Partial<LinkDraft> = {}): LinkDraft {
    return {
      namespace: 'todos',
      forwardLabel: 'owner',
      targetNamespace: '$users',
      reverseLabel: 'todos',
      forwardCardinality: 'one',
      reverseCardinality: 'many',
      cascade: true,
      ...patch,
    };
  }

  it('describes cardinality with the forward side and reverse uniqueness', () => {
    expect(linkConstraints(draft())).toEqual({
      cardinality: 'one',
      unique: false,
      cascade: true,
    });
    expect(
      linkConstraints(
        draft({ forwardCardinality: 'many', reverseCardinality: 'one' }),
      ),
    ).toEqual({ cardinality: 'many', unique: true, cascade: false });
  });

  it('validates labels, the target and self-links', () => {
    const input = { namespaces: [namespace, users], existingLabels: ['title'] };
    expect(linkDraftProblem(draft(), input)).toBeUndefined();
    expect(linkDraftProblem(draft({ forwardLabel: '' }), input)).toBe(
      'Enter a name.',
    );
    expect(linkDraftProblem(draft({ targetNamespace: 'missing' }), input)).toBe(
      'Choose a target namespace.',
    );
    expect(
      linkDraftProblem(
        draft({
          targetNamespace: 'todos',
          namespace: 'todos',
          forwardLabel: 'parent',
          reverseLabel: 'parent',
        }),
        input,
      ),
    ).toBe('A self-link needs different forward and reverse labels.');
    expect(linkDraftProblem(draft({ forwardLabel: 'title' }), input)).toBe(
      'That forward label already exists in this namespace.',
    );
  });

  it('adds the ref attribute with both identities', () => {
    const ids = ['attr-link', 'ident-forward', 'ident-reverse'];
    expect(addLinkOps(draft(), () => ids.shift() ?? 'extra')).toEqual([
      [
        'add-attr',
        {
          id: 'attr-link',
          'forward-identity': ['ident-forward', 'todos', 'owner'],
          'reverse-identity': ['ident-reverse', '$users', 'todos'],
          'value-type': 'ref',
          cardinality: 'one',
          'unique?': false,
          'index?': false,
          'required?': false,
          'on-delete': 'cascade',
        },
      ],
    ]);
  });

  it('reads both cardinalities and cascade flags back from the catalog', () => {
    const link: AppLink = {
      id: 'attr-owner',
      namespace: 'todos',
      label: 'owner',
      targetNamespace: '$users',
      reverseLabel: 'todos',
      cardinality: 'many',
      onDelete: undefined,
    };
    const raw: InstantAttr = {
      id: 'attr-owner',
      'forward-identity': ['ident', 'todos', 'owner'],
      'reverse-identity': ['ident-reverse', '$users', 'todos'],
      'index?': false,
      'unique?': true,
      cardinality: 'many',
      'value-type': 'ref',
      'on-delete-reverse': 'cascade',
    };

    expect(linkCardinalities(link, raw)).toEqual({
      forward: 'many',
      reverse: 'one',
    });
    expect(linkCardinalities(link, undefined).reverse).toBe('many');
    expect(linkCascades(link, raw)).toBe(true);
    expect(linkCascades(link, undefined)).toBe(false);
  });
});

describe('soft-deleted attributes', () => {
  it('maps the server row to the table view', () => {
    const attribute: SoftDeletedAttribute = {
      id: 'attr-1',
      'forward-identity': ['ident-1', 'todos', 'title'],
      'value-type': 'blob',
      cardinality: 'one',
      'checked-data-type': 'string',
      'deletion-marked-at': '2026-10-27T10:00:00Z',
    };
    expect(softDeletedAttributeView(attribute)).toEqual({
      id: 'attr-1',
      namespace: 'todos',
      name: 'title',
      type: 'string',
      deletedAt: '2026-10-27T10:00:00Z',
    });
  });

  it('strips the catalog marker the server stores on a deleted row', () => {
    const attribute: SoftDeletedAttribute = {
      id: 'attr-1',
      'forward-identity': [
        'ident-1',
        'attr-1_deleted$todos',
        'attr-1_deleted$notes',
      ],
      'value-type': 'blob',
      cardinality: 'one',
      'deletion-marked-at': '2026-10-27T10:00:00Z',
    };
    expect(softDeletedAttributeView(attribute)).toMatchObject({
      namespace: 'todos',
      name: 'notes',
    });
  });
});
