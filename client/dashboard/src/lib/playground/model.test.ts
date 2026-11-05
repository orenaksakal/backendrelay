import { describe, expect, it } from 'vitest';
import {
  actorLabel,
  countEntities,
  defaultPlaygroundQuery,
  entityLabel,
  firstNamespaceRows,
  lintQueryDocument,
  lintTransactionSteps,
  liveQueryError,
  parseQueryDocument,
  parseTransactionSteps,
  playgroundActor,
  queryData,
  queryStatus,
  sourceSummary,
  transactionStepProblems,
} from './model';

describe('playground query editor', () => {
  it('starts on the first user namespace, else $users', () => {
    expect(JSON.parse(defaultPlaygroundQuery(['todos', 'goals']))).toEqual({
      todos: { $: { limit: 10 } },
    });
    expect(JSON.parse(defaultPlaygroundQuery(['$users']))).toEqual({
      $users: { $: { limit: 10 } },
    });
    expect(JSON.parse(defaultPlaygroundQuery([]))).toEqual({
      $users: { $: { limit: 10 } },
    });
  });

  it('requires a JSON object', () => {
    expect(parseQueryDocument('{"todos": {}}')).toEqual({ todos: {} });
    expect(() => parseQueryDocument('[1, 2]')).toThrow(
      'The query must be a JSON object.',
    );
    expect(() => parseQueryDocument('{')).toThrow(
      'The query must be valid JSON.',
    );
    expect(lintQueryDocument('[1, 2]')[0]?.message).toBe(
      'The query must be a JSON object.',
    );
    expect(lintQueryDocument('{"todos": {}}')).toEqual([]);
  });
});

describe('playground transaction steps', () => {
  const update: unknown[] = ['update', 'todos', 'todo-1', { title: 'Done' }];

  it('accepts the Admin SDK tuple format', () => {
    expect(parseTransactionSteps(JSON.stringify([update]))).toEqual([update]);
    expect(
      parseTransactionSteps(
        JSON.stringify([['delete', 'todos', 'todo-1'], update]),
      ),
    ).toHaveLength(2);
    expect(
      parseTransactionSteps(
        JSON.stringify([['update', 'todos', ['email', 'ada@example.com'], {}]]),
      ),
    ).toHaveLength(1);
  });

  it('explains the common mistakes', () => {
    expect(transactionStepProblems([])).toEqual([
      'Add at least one transaction step.',
    ]);
    expect(transactionStepProblems([['nope', 'todos', 'todo-1', {}]])[0]).toBe(
      'Step 1 uses an unknown action. Use create, update, merge, delete, link, unlink or ruleParams.',
    );
    expect(transactionStepProblems([['update', 'todos']])[0]).toBe(
      'Step 1 must be ["action", "namespace", "id", args?].',
    );
    expect(transactionStepProblems([['update', 'todos', 'todo-1']])[0]).toBe(
      'Step 1 needs its arguments object.',
    );
    expect(() => parseTransactionSteps('{"steps": []}')).toThrow(
      'The transaction steps must be a JSON array.',
    );
    expect(lintTransactionSteps('[]')[0]?.message).toBe(
      'Add at least one transaction step.',
    );
  });
});

describe('playground results', () => {
  const live = {
    data: {
      todos: [
        { id: 'todo-1', title: 'Buy milk' },
        { id: 'todo-2', title: 'Walk' },
      ],
      goals: [{ id: 'goal-1' }],
    },
    pageInfo: { todos: { hasNextPage: false } },
  };

  it('unwraps live and debug shapes', () => {
    expect(queryData(live)).toEqual(live.data);
    expect(queryData({ todos: [] })).toEqual({ todos: [] });
    expect(countEntities(live)).toBe(3);
    expect(entityLabel(1)).toBe('1 entity');
    expect(queryStatus(2, 12.4)).toBe('2 entities · 12 ms');
  });

  it('tabulates the first namespace with id first', () => {
    const rows = firstNamespaceRows(live);
    expect(rows?.namespace).toBe('todos');
    expect(rows?.rows).toHaveLength(2);
    expect(rows?.columns).toEqual(['id', 'title']);

    expect(firstNamespaceRows({ todos: [] })).toEqual({
      namespace: 'todos',
      rows: [],
      columns: [],
    });
    expect(firstNamespaceRows({ pageInfo: {} })).toBeNull();
  });

  it('reads a live error and a source preview', () => {
    expect(liveQueryError({ error: { message: 'Permission denied' } })).toBe(
      'Permission denied',
    );
    expect(liveQueryError({ data: {} })).toBeNull();
    expect(sourceSummary('\n  {"todos": {}}\n')).toBe('{"todos": {}}');
    expect(sourceSummary('   ')).toBe('Empty document');
  });
});

describe('playground actors', () => {
  it('maps the Run as controls to request actors', () => {
    expect(playgroundActor('admin', '', '')).toEqual({ kind: 'admin' });
    expect(playgroundActor('guest', '', '')).toEqual({ kind: 'guest' });
    expect(playgroundActor('email', ' ada@example.com ', '')).toEqual({
      kind: 'user',
      email: 'ada@example.com',
    });
    expect(playgroundActor('email', '  ', '')).toBe(
      'Choose a user, or enter their email.',
    );
    expect(playgroundActor('token', '', ' tok ')).toEqual({
      kind: 'token',
      token: 'tok',
    });
    expect(playgroundActor('token', '', '')).toBe(
      'Paste a refresh token, or run as a guest.',
    );
  });

  it('labels the actor in history', () => {
    expect(actorLabel('admin', '')).toBe('Admin');
    expect(actorLabel('guest', '')).toBe('Guest');
    expect(actorLabel('email', 'ada@example.com')).toBe('ada@example.com');
    expect(actorLabel('token', '')).toBe('Token');
  });
});
