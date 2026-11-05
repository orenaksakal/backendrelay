import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import {
  clearDraft,
  draftKey,
  emptyDraft,
  NEW_ENTITY_DRAFT_ID,
  readDraft,
  writeDraft,
} from './drafts';

const DRAFT = {
  values: { title: 'Buy milk', done: false, count: 3 },
  unset: ['note'],
  json: { meta: '{"a":1}' },
  links: { 'attr-owner': { add: ['u1'], remove: ['u2'] } },
};

beforeEach(() => {
  window.sessionStorage.clear();
});

afterEach(() => {
  window.sessionStorage.clear();
});

describe('entity drafts', () => {
  it('round-trips the sheet state per entity', () => {
    writeDraft('app-1', 'todos', 't1', DRAFT);

    expect(readDraft('app-1', 'todos', 't1')).toEqual(DRAFT);
    expect(
      window.sessionStorage.getItem(draftKey('app-1', 'todos', 't1')),
    ).toBe(JSON.stringify(DRAFT));
  });

  it('keeps drafts apart by app, namespace and entity', () => {
    writeDraft('app-1', 'todos', 't1', DRAFT);

    expect(readDraft('app-2', 'todos', 't1')).toBeNull();
    expect(readDraft('app-1', 'goals', 't1')).toBeNull();
    expect(readDraft('app-1', 'todos', 't2')).toBeNull();
  });

  it('reads a new-entity draft from the shared new slot', () => {
    writeDraft('app-1', 'todos', NEW_ENTITY_DRAFT_ID, DRAFT);

    expect(readDraft('app-1', 'todos', NEW_ENTITY_DRAFT_ID)).toEqual(DRAFT);
    expect(
      window.sessionStorage.getItem(
        draftKey('app-1', 'todos', NEW_ENTITY_DRAFT_ID),
      ),
    ).not.toBeNull();
  });

  it('ignores a corrupted or outdated entry', () => {
    window.sessionStorage.setItem(draftKey('app-1', 'todos', 't1'), '{');
    expect(readDraft('app-1', 'todos', 't1')).toBeNull();

    window.sessionStorage.setItem(
      draftKey('app-1', 'todos', 't1'),
      JSON.stringify({ values: { title: 42 } }),
    );
    expect(readDraft('app-1', 'todos', 't1')).toBeNull();
  });

  it('clears one draft without touching the others', () => {
    writeDraft('app-1', 'todos', 't1', DRAFT);
    writeDraft('app-1', 'todos', 't2', DRAFT);

    clearDraft('app-1', 'todos', 't1');

    expect(readDraft('app-1', 'todos', 't1')).toBeNull();
    expect(readDraft('app-1', 'todos', 't2')).toEqual(DRAFT);
  });

  it('has an empty draft shape for the sheet', () => {
    expect(emptyDraft()).toEqual({
      values: {},
      unset: [],
      json: {},
      links: {},
    });
  });
});
