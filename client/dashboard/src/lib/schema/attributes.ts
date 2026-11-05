import type {
  IndexingJobType,
  SoftDeletedAttribute,
} from '@/lib/api/endpoints/apps';
import type {
  AppAttribute,
  AppLink,
  AppNamespace,
  AttributeType,
  InstantAttr,
} from '@/lib/instant';

/**
 * C.7 attribute and link edits. Everything here is pure: the sheet and the
 * tabs build the transaction steps and constraint jobs, then send them through
 * the app's Instant client and the dashboard API.
 */

export const ATTRIBUTE_TYPE_OPTIONS: ReadonlyArray<{
  value: AttributeType;
  label: string;
  description: string;
}> = [
  {
    value: 'string',
    label: 'String',
    description: 'Text. The server checks that existing values are strings.',
  },
  {
    value: 'number',
    label: 'Number',
    description: 'A finite number. Dates are stored separately.',
  },
  {
    value: 'boolean',
    label: 'Boolean',
    description: 'True or false.',
  },
  {
    value: 'date',
    label: 'Date',
    description: 'An epoch-millisecond timestamp.',
  },
  {
    value: 'json',
    label: 'JSON',
    description: 'Any JSON value. Stored without a type check.',
  },
  {
    value: 'any',
    label: 'Any',
    description: 'No type check; values keep whatever type the app writes.',
  },
];

export interface AttributeDraft {
  name: string;
  type: AttributeType;
  required: boolean;
  unique: boolean;
  indexed: boolean;
}

export function draftFromAttribute(attribute: AppAttribute): AttributeDraft {
  return {
    name: attribute.name,
    type: attribute.type,
    required: attribute.isRequired,
    unique: attribute.isUnique,
    indexed: attribute.isIndexed,
  };
}

const NAME_PATTERN = /^[A-Za-z0-9_-]+$/;

function labelProblem(label: string): string | undefined {
  if (label.trim() === '') return 'Enter a name.';
  if (label.startsWith('$')) return 'Names cannot start with $.';
  if (!NAME_PATTERN.test(label)) return 'Use only letters, numbers, _ and -.';
  return undefined;
}

/**
 * The add/edit sheet's name check. `current` is the attribute's existing name,
 * so an unchanged name is not a duplicate.
 */
export function attributeNameProblem(
  value: string,
  existing: ReadonlyArray<string> = [],
  current?: string,
): string | undefined {
  const name = value.trim();
  const problem = labelProblem(name);
  if (problem !== undefined) return problem;
  if (current !== undefined && name === current) return undefined;
  if (name === 'id') return 'id is added automatically and cannot be renamed.';
  if (existing.includes(name)) return 'That attribute already exists.';
  return undefined;
}

/** The enforced type an attribute stores, or `undefined` for an unchecked one. */
export function checkedDataTypeFor(
  type: AttributeType,
): 'string' | 'number' | 'boolean' | 'date' | undefined {
  if (type === 'json' || type === 'any') return undefined;
  return type;
}

/* ------------------------------------------------------------------------- */
/* Transaction steps                                                          */
/* ------------------------------------------------------------------------- */

export function addAttributeOps(
  namespace: string,
  draft: AttributeDraft,
  nextId: () => string = () => crypto.randomUUID(),
): unknown[][] {
  const attribute: Record<string, unknown> = {
    id: nextId(),
    'forward-identity': [nextId(), namespace, draft.name.trim()],
    'value-type': 'blob',
    cardinality: 'one',
    'unique?': draft.unique,
    'index?': draft.indexed,
    'required?': draft.required,
  };
  const checked = checkedDataTypeFor(draft.type);
  if (checked !== undefined) attribute['checked-data-type'] = checked;
  return [['add-attr', attribute]];
}

/**
 * Renaming keeps the attribute's identity id and namespace; the server rejects
 * a new name that collides with another attribute.
 */
export function renameAttributeOps(
  attribute: AppAttribute,
  nextName: string,
  raw: InstantAttr | undefined,
): unknown[][] {
  const name = nextName.trim();
  if (name === attribute.name || raw === undefined) return [];
  const forward = raw['forward-identity'];
  return [
    [
      'update-attr',
      {
        id: attribute.id,
        'forward-identity': [forward[0], attribute.namespace, name],
      },
    ],
  ];
}

export function deleteAttributeOps(
  attribute: Pick<AppAttribute, 'id'>,
): unknown[][] {
  return [['delete-attr', attribute.id]];
}

/** Deleting a namespace deletes every attribute and link that defines it. */
export function deleteNamespaceOps(namespace: AppNamespace): unknown[][] {
  const ids = new Set<string>();
  for (const attribute of namespace.attributes) ids.add(attribute.id);
  for (const link of namespace.links) ids.add(link.id);
  return [...ids].map((id) => ['delete-attr', id]);
}

export function restoreAttributeOps(attributeId: string): unknown[][] {
  return [['restore-attr', attributeId]];
}

export interface ConstraintJob {
  jobType: IndexingJobType;
  checkedDataType?: string;
}

/**
 * C.7: a saved constraint that can invalidate existing data is checked by an
 * indexing job instead of an immediate `update-attr`. A new attribute has no
 * data to check, so it returns no jobs.
 */
export function pendingConstraintJobs(
  previous: AttributeDraft | null,
  next: AttributeDraft,
): ConstraintJob[] {
  if (previous === null) return [];
  const jobs: ConstraintJob[] = [];
  if (previous.required !== next.required) {
    jobs.push({ jobType: next.required ? 'required' : 'remove-required' });
  }
  if (previous.indexed !== next.indexed) {
    jobs.push({ jobType: next.indexed ? 'index' : 'remove-index' });
  }
  if (previous.unique !== next.unique) {
    jobs.push({ jobType: next.unique ? 'unique' : 'remove-unique' });
  }
  if (previous.type !== next.type) {
    const checked = checkedDataTypeFor(next.type);
    jobs.push(
      checked === undefined
        ? { jobType: 'remove-data-type' }
        : { jobType: 'check-data-type', checkedDataType: checked },
    );
  }
  return jobs;
}

/* ------------------------------------------------------------------------- */
/* Links                                                                      */
/* ------------------------------------------------------------------------- */

export type LinkCardinality = 'one' | 'many';

export interface LinkDraft {
  /** The namespace the link starts from; fixed by the selected one. */
  namespace: string;
  forwardLabel: string;
  targetNamespace: string;
  reverseLabel: string;
  forwardCardinality: LinkCardinality;
  reverseCardinality: LinkCardinality;
  cascade: boolean;
}

export interface LinkValidationInput {
  namespaces: ReadonlyArray<AppNamespace>;
  /** Attribute labels already used by the from-namespace. */
  existingLabels: ReadonlyArray<string>;
}

export function linkDraftProblem(
  draft: LinkDraft,
  { namespaces, existingLabels }: LinkValidationInput,
): string | undefined {
  const forward = labelProblem(draft.forwardLabel);
  if (forward !== undefined) return forward;
  const reverse = labelProblem(draft.reverseLabel);
  if (reverse !== undefined) return reverse;
  if (
    !namespaces.some((namespace) => namespace.name === draft.targetNamespace)
  ) {
    return 'Choose a target namespace.';
  }
  if (
    draft.targetNamespace === draft.namespace &&
    draft.forwardLabel.trim() === draft.reverseLabel.trim()
  ) {
    return 'A self-link needs different forward and reverse labels.';
  }
  if (existingLabels.includes(draft.forwardLabel.trim())) {
    return 'That forward label already exists in this namespace.';
  }
  return undefined;
}

export interface LinkConstraints {
  cardinality: LinkCardinality;
  unique: boolean;
  cascade: boolean;
}

/**
 * InstantDB describes a relation with the forward cardinality plus `unique?`
 * on the reverse side. `unique?` true means the reverse has one.
 */
export function linkConstraints(draft: LinkDraft): LinkConstraints {
  return {
    cardinality: draft.forwardCardinality,
    unique: draft.reverseCardinality === 'one',
    // Cascade is only meaningful when the from-namespace has one target.
    cascade: draft.cascade && draft.forwardCardinality === 'one',
  };
}

export function addLinkOps(
  draft: LinkDraft,
  nextId: () => string = () => crypto.randomUUID(),
): unknown[][] {
  const constraints = linkConstraints(draft);
  const attribute: Record<string, unknown> = {
    id: nextId(),
    'forward-identity': [nextId(), draft.namespace, draft.forwardLabel.trim()],
    'reverse-identity': [
      nextId(),
      draft.targetNamespace,
      draft.reverseLabel.trim(),
    ],
    'value-type': 'ref',
    cardinality: constraints.cardinality,
    'unique?': constraints.unique,
    'index?': false,
    'required?': false,
  };
  if (constraints.cascade) attribute['on-delete'] = 'cascade';
  return [['add-attr', attribute]];
}

export interface LinkCardinalities {
  forward: LinkCardinality;
  reverse: LinkCardinality;
}

export function linkCardinalities(
  link: AppLink,
  raw: InstantAttr | undefined,
): LinkCardinalities {
  return {
    forward: link.cardinality,
    reverse: raw?.['unique?'] === true ? 'one' : 'many',
  };
}

/** The Deleted tab and the links table both read the raw catalog. */
export function linkCascades(
  link: AppLink,
  raw: InstantAttr | undefined,
): boolean {
  return (
    link.onDelete === 'cascade' || raw?.['on-delete-reverse'] === 'cascade'
  );
}

/* ------------------------------------------------------------------------- */
/* Soft-deleted attributes                                                    */
/* ------------------------------------------------------------------------- */

export interface SoftDeletedAttributeView {
  id: string;
  namespace: string;
  name: string;
  type: AttributeType;
  deletedAt: string | undefined;
}

/**
 * A soft-deleted row stores `<attr-id>_deleted$<name>` in the catalog so the
 * live attributes cannot collide with it. The dashboard API returns that raw
 * row, and the reference explorer strips the prefix the same way.
 */
function withoutDeletedMarker(value: string, attributeId: string): string {
  const prefix = `${attributeId}_deleted$`;
  return value.startsWith(prefix) ? value.slice(prefix.length) : value;
}

export function softDeletedAttributeView(
  attribute: SoftDeletedAttribute,
): SoftDeletedAttributeView {
  const [, namespace, name] = attribute['forward-identity'];
  return {
    id: attribute.id,
    namespace: withoutDeletedMarker(namespace, attribute.id),
    name: withoutDeletedMarker(name, attribute.id),
    type:
      (attribute['checked-data-type'] as AttributeType | undefined) ?? 'any',
    deletedAt: attribute['deletion-marked-at'],
  };
}
