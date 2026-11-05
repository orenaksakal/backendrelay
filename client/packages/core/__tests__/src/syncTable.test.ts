import { expect, test } from 'vitest';
import { CallbackEventType, SyncTable } from '../../src/SyncTable';
import InMemoryStorage from '../../src/InMemoryStorage';
import { AttrsStoreClass, createStore } from '../../src/store';

const attrsStore = new AttrsStoreClass(
  {
    'posts-id': {
      id: 'posts-id',
      'value-type': 'blob',
      cardinality: 'one',
      'forward-identity': ['posts-id-ident', 'posts', 'id'],
      'checked-data-type': 'string',
    },
    'posts-title': {
      id: 'posts-title',
      'value-type': 'blob',
      cardinality: 'one',
      'forward-identity': ['posts-title-ident', 'posts', 'title'],
      'checked-data-type': 'string',
    },
    'users-id': {
      id: 'users-id',
      'value-type': 'blob',
      cardinality: 'one',
      'forward-identity': ['users-id-ident', 'users', 'id'],
      'checked-data-type': 'string',
    },
    'users-name': {
      id: 'users-name',
      'value-type': 'blob',
      cardinality: 'one',
      'forward-identity': ['users-name-ident', 'users', 'name'],
      'checked-data-type': 'string',
    },
  },
  null,
);

function createSyncTable() {
  const sent: { eventId: string; msg: any }[] = [];
  const table = new SyncTable(
    (eventId, msg) => sent.push({ eventId, msg }),
    new InMemoryStorage('test-app', 'syncSubs'),
    { useDateObjects: false },
    { debug: () => {}, error: () => {}, info: () => {} },
    (triples) => createStore(attrsStore, triples),
    () => attrsStore,
  );
  return { table, sent };
}

async function waitForSubscription() {
  await new Promise((resolve) => setTimeout(resolve));
}

test('SyncTable combines multi-root snapshot batches', async () => {
  const { table, sent } = createSyncTable();
  const query = { posts: {}, users: {} };
  const events: any[] = [];
  table.subscribe(query, (event) => events.push(event));
  await waitForSubscription();
  table.onStartSyncOk({
    'subscription-id': 'sub',
    'client-event-id': sent[0].eventId,
    q: query,
    token: 'token',
  });

  table.onSyncLoadBatch({
    'subscription-id': 'sub',
    'join-rows': [
      [
        ['post-1', 'posts-id', 'post-1', 1],
        ['post-1', 'posts-title', 'hello', 1],
      ],
    ],
  });
  table.onSyncLoadBatch({
    'subscription-id': 'sub',
    'join-rows': [
      [
        ['user-1', 'users-id', 'user-1', 1],
        ['user-1', 'users-name', 'Ada', 1],
      ],
    ],
  });

  expect(events.at(-1)).toMatchObject({
    type: CallbackEventType.InitialSyncBatch,
    data: {
      posts: [{ id: 'post-1', title: 'hello' }],
      users: [{ id: 'user-1', name: 'Ada' }],
    },
    batch: [{ id: 'user-1', name: 'Ada' }],
  });
});

test('SyncTable applies legacy single-root updates to the combined store', async () => {
  const { table, sent } = createSyncTable();
  const query = { posts: {} };
  const events: any[] = [];
  table.subscribe(query, (event) => events.push(event));
  await waitForSubscription();
  table.onStartSyncOk({
    'subscription-id': 'sub',
    'client-event-id': sent[0].eventId,
    q: query,
    token: 'token',
  });
  table.onSyncLoadBatch({
    'subscription-id': 'sub',
    'join-rows': [
      [
        ['post-1', 'posts-id', 'post-1', 1],
        ['post-1', 'posts-title', 'before', 1],
      ],
    ],
  });
  table.onSyncInitFinish({ 'subscription-id': 'sub', 'tx-id': 1 });

  table.onSyncUpdateTriples({
    'subscription-id': 'sub',
    txes: [
      {
        'tx-id': 2,
        changes: [
          { action: 'removed', triple: ['post-1', 'posts-title', 'before', 1] },
          { action: 'added', triple: ['post-1', 'posts-title', 'after', 2] },
        ],
      },
    ],
  });

  expect(events.at(-1)).toMatchObject({
    type: CallbackEventType.SyncTransaction,
    data: { posts: [{ id: 'post-1', title: 'after' }] },
    added: [],
    removed: [],
    updated: [
      {
        oldEntity: { id: 'post-1', title: 'before' },
        newEntity: { id: 'post-1', title: 'after' },
        changedFields: {
          title: { oldValue: 'before', newValue: 'after' },
        },
      },
    ],
  });
});

test('SyncTable reuses an active subscription for duplicate callbacks', async () => {
  const { table, sent } = createSyncTable();
  const query = { posts: {} };
  const first: any[] = [];
  const second: any[] = [];
  table.subscribe(query, (event) => first.push(event));
  await waitForSubscription();
  table.onStartSyncOk({
    'subscription-id': 'sub',
    'client-event-id': sent[0].eventId,
    q: query,
    token: 'token',
  });
  table.onSyncLoadBatch({
    'subscription-id': 'sub',
    'join-rows': [
      [
        ['post-1', 'posts-id', 'post-1', 1],
        ['post-1', 'posts-title', 'hello', 1],
      ],
    ],
  });
  table.onSyncInitFinish({ 'subscription-id': 'sub', 'tx-id': 1 });

  table.subscribe(query, (event) => second.push(event));
  await waitForSubscription();

  expect(sent.filter(({ msg }) => msg.op === 'start-sync')).toHaveLength(1);
  expect(sent.filter(({ msg }) => msg.op === 'remove-sync')).toHaveLength(0);
  expect(second).toMatchObject([
    {
      type: CallbackEventType.LoadFromStorage,
      data: { posts: [{ id: 'post-1', title: 'hello' }] },
    },
  ]);
  expect(first).toHaveLength(2);
});

test('SyncTable cleans up stale start acknowledgements and ignores their frames', async () => {
  const { table, sent } = createSyncTable();
  const query = { posts: {} };
  const events: any[] = [];
  table.subscribe(query, (event) => events.push(event));
  await waitForSubscription();
  const retiredStart = sent[0].eventId;

  await table.flushPending();
  const currentStart = sent.at(-1).eventId;
  table.onStartSyncOk({
    'subscription-id': 'retired',
    'client-event-id': retiredStart,
    q: query,
    token: 'retired-token',
  });
  table.onSyncLoadBatch({
    'subscription-id': 'retired',
    'join-rows': [[['post-1', 'posts-id', 'post-1', 1]]],
  });
  table.onStartSyncOk({
    'subscription-id': 'current',
    'client-event-id': currentStart,
    q: query,
    token: 'current-token',
  });
  table.onSyncLoadBatch({
    'subscription-id': 'current',
    'join-rows': [
      [
        ['post-2', 'posts-id', 'post-2', 1],
        ['post-2', 'posts-title', 'current', 1],
      ],
    ],
  });

  expect(sent.at(-1)).toMatchObject({
    msg: { op: 'remove-sync', 'subscription-id': 'retired' },
  });
  expect(events).toMatchObject([
    {
      type: CallbackEventType.InitialSyncBatch,
      data: { posts: [{ id: 'post-2', title: 'current' }] },
    },
  ]);
});
