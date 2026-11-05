import { describe, expect, it } from 'vitest';
import type { AppAttribute, AppInstantClient, AppTxChunk } from '@/lib/instant';
import {
  collectEntityFields,
  deleteEntities,
  entityExport,
  fromDateTimeLocal,
  initialFieldValues,
  isInlineEditable,
  linkUpdateValue,
  namespaceNameProblem,
  newNamespaceOps,
  parseCellValue,
  planEntityWrite,
  runEntityWrite,
  toDateTimeLocal,
  updateCell,
  type FieldValues,
} from './writes';

function attribute(
  name: string,
  type: AppAttribute['type'],
  overrides: Partial<AppAttribute> = {},
): AppAttribute {
  return {
    id: `attr-${name}`,
    namespace: 'todos',
    name,
    type,
    isIndexed: false,
    isUnique: false,
    isRequired: false,
    isPrimary: false,
    ...overrides,
  };
}

const ATTRIBUTES: AppAttribute[] = [
  attribute('id', 'string', { isUnique: true, isPrimary: true }),
  attribute('title', 'string', { isRequired: true, isIndexed: true }),
  attribute('done', 'boolean'),
  attribute('count', 'number'),
  attribute('at', 'date'),
  attribute('meta', 'json'),
  attribute('note', 'string'),
];

interface RecordedCall {
  op: string;
  values: unknown;
}

function fakeClient() {
  const calls: RecordedCall[] = [];
  const unsetOps: unknown[][] = [];
  const transactions: unknown[] = [];
  const chunk: AppTxChunk = {
    update: (values) => {
      calls.push({ op: 'update', values });
      return chunk;
    },
    unlink: (values) => {
      calls.push({ op: 'unlink', values });
      return chunk;
    },
    link: (values) => {
      calls.push({ op: 'link', values });
      return chunk;
    },
    delete: () => {
      calls.push({ op: 'delete', values: null });
      return chunk;
    },
  };
  const client: AppInstantClient = {
    core: {
      shutdown: () => undefined,
      subscribeQuery: () => () => undefined,
      _reactor: {
        subscribeAttrs: () => () => undefined,
        pushOps: (ops) => {
          unsetOps.push(...ops.map((op) => [...op]));
          return Promise.resolve();
        },
      },
    },
    tx: { todos: { t1: chunk } },
    transact: (chunks) => {
      transactions.push(chunks);
      return Promise.resolve();
    },
  };
  return { client, calls, unsetOps, transactions };
}

describe('inline edits', () => {
  it('allows only string, number, boolean and date attributes', () => {
    expect(isInlineEditable(attribute('title', 'string'))).toBe(true);
    expect(isInlineEditable(attribute('count', 'number'))).toBe(true);
    expect(isInlineEditable(attribute('done', 'boolean'))).toBe(true);
    expect(isInlineEditable(attribute('at', 'date'))).toBe(true);
    expect(isInlineEditable(attribute('meta', 'json'))).toBe(false);
    expect(isInlineEditable(attribute('id', 'string'))).toBe(false);
  });

  it('coerces a number and rejects what is not one', () => {
    expect(parseCellValue(attribute('count', 'number'), '42', 1)).toEqual({
      ok: true,
      value: 42,
    });
    expect(parseCellValue(attribute('count', 'number'), '', 1).ok).toBe(false);
    expect(parseCellValue(attribute('count', 'number'), 'many', 1).ok).toBe(
      false,
    );
  });

  it('accepts true/false spellings for a boolean cell', () => {
    expect(parseCellValue(attribute('done', 'boolean'), 'true', false)).toEqual(
      { ok: true, value: true },
    );
    expect(parseCellValue(attribute('done', 'boolean'), '0', true)).toEqual({
      ok: true,
      value: false,
    });
    expect(parseCellValue(attribute('done', 'boolean'), 'maybe', true).ok).toBe(
      false,
    );
  });

  it('keeps the date shape the existing value used', () => {
    const iso = '2026-01-02T03:04:00.000Z';
    expect(parseCellValue(attribute('at', 'date'), iso, iso)).toEqual({
      ok: true,
      value: iso,
    });
    expect(
      parseCellValue(attribute('at', 'date'), iso, Date.parse(iso)),
    ).toEqual({ ok: true, value: Date.parse(iso) });
    expect(parseCellValue(attribute('at', 'date'), 'soon', iso).ok).toBe(false);
  });

  it('converts a date to and from a datetime-local control', () => {
    const value = new Date(2026, 0, 2, 3, 4);
    const local = toDateTimeLocal(value);
    expect(local).toBe('2026-01-02T03:04');

    const parsed = fromDateTimeLocal(local);
    expect(parsed.ok).toBe(true);
    if (parsed.ok) {
      expect(new Date(parsed.value as string).getTime()).toBe(value.getTime());
    }
    expect(fromDateTimeLocal('')).toEqual({ ok: true, value: null });
  });

  it('starts missing fields unset instead of inventing empty values', () => {
    const fields = initialFieldValues(ATTRIBUTES, { id: 't1' });
    expect(fields['title']).toMatchObject({
      value: undefined,
      unset: false,
    });
    expect(fields['done']).toMatchObject({
      value: undefined,
      unset: false,
    });
    expect(fields['meta']).toMatchObject({
      value: undefined,
      unset: false,
      json: undefined,
    });
  });
});

describe('entity form values', () => {
  function filled(): FieldValues {
    return {
      title: { value: 'Buy milk', unset: false },
      done: { value: false, unset: false },
      count: { value: '3', unset: false },
      at: { value: '2026-01-02T03:04', unset: false },
      meta: { value: undefined, unset: false, json: '{"a":1}' },
      note: { value: undefined, unset: false },
    };
  }

  it('collects typed values, JSON and dates', () => {
    const collected = collectEntityFields(ATTRIBUTES, filled());

    expect(collected.errors).toEqual({});
    expect(collected.unset).toEqual([]);
    expect(collected.values['title']).toBe('Buy milk');
    expect(collected.values['done']).toBe(false);
    expect(collected.values['count']).toBe(3);
    expect(collected.values['meta']).toEqual({ a: 1 });
    expect(new Date(collected.values['at'] as string).getTime()).toBe(
      new Date(2026, 0, 2, 3, 4).getTime(),
    );
    expect('note' in collected.values).toBe(false);
  });

  it('reports missing required fields and invalid JSON or numbers', () => {
    const missingTitle = collectEntityFields(ATTRIBUTES, {
      ...filled(),
      title: { value: '', unset: false },
    });
    expect(missingTitle.errors['title']).toBe('This field is required.');

    const brokenJson = collectEntityFields(ATTRIBUTES, {
      ...filled(),
      meta: { value: undefined, unset: false, json: '{' },
    });
    expect(brokenJson.errors['meta']).toBe('Enter valid JSON.');

    const brokenNumber = collectEntityFields(ATTRIBUTES, {
      ...filled(),
      count: { value: 'many', unset: false },
    });
    expect(brokenNumber.errors['count']).toBe('Enter a number.');
  });

  it('turns Clear (unset) into a retract, but blocks a required one', () => {
    const note = collectEntityFields(ATTRIBUTES, {
      ...filled(),
      note: { value: 'kept', unset: true, original: 'kept' },
    });
    expect(note.unset).toEqual([{ id: 'attr-note', value: 'kept' }]);

    const title = collectEntityFields(ATTRIBUTES, {
      ...filled(),
      title: { value: 'Buy milk', unset: true },
    });
    expect(title.errors['title']).toBe('This field is required.');
  });

  it('leaves an untouched optional number or date alone', () => {
    const collected = collectEntityFields(ATTRIBUTES, {
      title: { value: 'Buy milk', unset: false },
    });
    expect(collected.unset).toEqual([]);
    expect('count' in collected.values).toBe(false);
    expect('at' in collected.values).toBe(false);
  });
});

describe('entity writes', () => {
  it('builds one chunk with unlinks, the update and links', () => {
    const { client, calls } = fakeClient();
    const plan = planEntityWrite(client, {
      namespace: 'todos',
      entityId: 't1',
      values: { title: 'Ship v2' },
      unset: [{ id: 'attr-note', value: 'kept' }],
      addLinks: { goals: ['g1'] },
      removeLinks: { owner: 'u1' },
    });

    expect(plan.chunks).toHaveLength(1);
    expect(plan.unsetSteps).toEqual([
      ['retract-triple', 't1', 'attr-note', 'kept'],
    ]);
    expect(calls.map((call) => call.op)).toEqual(['unlink', 'update', 'link']);
    expect(calls[0]?.values).toEqual({ owner: 'u1' });
    expect(calls[1]?.values).toEqual({ title: 'Ship v2' });
    expect(calls[2]?.values).toEqual({ goals: ['g1'] });
  });

  it('sends the chunk and then the unset retracts', async () => {
    const { client, unsetOps, transactions } = fakeClient();
    const plan = planEntityWrite(client, {
      namespace: 'todos',
      entityId: 't1',
      values: { title: 'Ship v2' },
      unset: [{ id: 'attr-note', value: 'kept' }],
      addLinks: {},
      removeLinks: {},
    });

    await runEntityWrite(client, plan);

    expect(transactions).toHaveLength(1);
    expect(unsetOps).toEqual([['retract-triple', 't1', 'attr-note', 'kept']]);
  });

  it('deletes entities and updates one cell with the app client', async () => {
    const { client, calls, transactions } = fakeClient();

    await deleteEntities(client, 'todos', ['t1']);
    await updateCell(client, 'todos', 't1', 'done', true);

    expect(calls.map((call) => call.op)).toEqual(['delete', 'update']);
    expect(calls[1]?.values).toEqual({ done: true });
    expect(transactions).toHaveLength(2);
  });

  it('normalises a link value for its cardinality', () => {
    expect(linkUpdateValue('one', ['u1'])).toBe('u1');
    expect(linkUpdateValue('many', ['g1', 'g2'])).toEqual(['g1', 'g2']);
  });
});

describe('bulk export', () => {
  it('names the file after the namespace and the day', () => {
    const file = entityExport(
      'todos',
      [{ id: 't1', title: 'Buy milk' }],
      new Date(2026, 0, 2),
    );
    expect(file.filename).toBe('todos-2026-01-02.json');
    expect(JSON.parse(file.json)).toEqual({
      todos: [{ id: 't1', title: 'Buy milk' }],
    });
  });
});

describe('namespaces', () => {
  it('accepts letters, numbers, _ and -, but not a $ prefix', () => {
    expect(namespaceNameProblem('todos')).toBeUndefined();
    expect(namespaceNameProblem('todo_2-items')).toBeUndefined();
    expect(namespaceNameProblem('')).toBe('Enter a namespace name.');
    expect(namespaceNameProblem('$users')).toBe(
      'Namespace names cannot start with $.',
    );
    expect(namespaceNameProblem('two words')).toBe(
      'Use only letters, numbers, _ and -.',
    );
    expect(namespaceNameProblem('todos', ['todos'])).toBe(
      'That namespace already exists.',
    );
  });

  it('adds the id attribute that creates the namespace', () => {
    const ids = ['attr-id', 'ident-id'];
    const ops = newNamespaceOps('todos', () => ids.shift() ?? 'extra');

    expect(ops).toEqual([
      [
        'add-attr',
        {
          id: 'attr-id',
          'forward-identity': ['ident-id', 'todos', 'id'],
          'value-type': 'blob',
          cardinality: 'one',
          'unique?': true,
          'index?': false,
        },
      ],
    ]);
  });
});
