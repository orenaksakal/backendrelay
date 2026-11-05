import { PersistedObject } from './utils/PersistedObject.ts';
import * as s from './store.ts';
import weakHash from './utils/weakHash.ts';
import uuid from './utils/id.ts';
import { Logger } from './Reactor.js';
import instaql from './instaql.ts';
import { InstaQLResponse, ValidQuery } from './queryTypes.ts';
import { EntitiesDef, IContainEntitiesAndLinks } from './schemaTypes.ts';
import { StoreInterface } from './index.ts';

type SubState = {
  txId?: number;
  subscriptionId: string;
  token: string;
};

type SubEntity = {
  entity: any;
  store: s.Store;
  serverCreatedAt: number;
};

type SubValues = {
  attrsStore: s.AttrsStore;
  entities: Array<SubEntity>;
};

type Sub = {
  query: any;
  hash: string;
  startEventId?: string;
  state?: SubState;
  values?: SubValues;
  createdAt: number;
  updatedAt: number;
};

type SubEntityInStorage = {
  entity: any;
  store: s.StoreJson;
  serverCreatedAt: number;
};

type SubValuesInStorage = {
  attrsStore: s.AttrsStoreJson;
  entities: Array<SubEntityInStorage>;
};

// We could make a better type for this if we had a return type for s.toJSON
type SubInStorage = Omit<Sub, 'values'> & {
  values: SubValuesInStorage;
};

type StartMsg = {
  op: 'start-sync';
  q: string;
};

type EndMsg = {
  op: 'remove-sync';
  'subscription-id': string;
  'keep-subscription': boolean;
};

type ResyncMsg = {
  op: 'resync-table';
  'subscription-id': string;
  'tx-id': number;
  token: string;
};

type SendMsg = StartMsg | EndMsg;

type StartSyncOkMsg = {
  'subscription-id': string;
  'client-event-id': string;
  q: string;
  token: string;
};

type Triple = [string, string, any, number];

type SyncLoadBatchMsg = {
  'subscription-id': string;
  'join-rows': Array<Triple[]>;
};

type SyncInitFinishMsg = {
  'subscription-id': string;
  'tx-id': number;
};

type SyncUpdateTriplesMsg = {
  'subscription-id': string;
  txes: {
    'tx-id': number;
    changes: { action: 'added' | 'removed'; triple: Triple }[];
  }[];
};

type TrySend = (eventId: string, msg: SendMsg) => void;

type Config = { useDateObjects: boolean };

// Modifies the data in place because it comes directly from storage
function syncSubFromStorage(sub: SubInStorage, useDateObjects: boolean): Sub {
  const values = sub.values;
  if (values) {
    const attrsStore = s.attrsStoreFromJSON(values.attrsStore, null);
    if (attrsStore) {
      for (const e of values.entities || []) {
        e.store.useDateObjects = useDateObjects;
        (e as unknown as SubEntity).store = s.fromJSON(attrsStore, e.store);
      }
      (values as unknown as SubValues).attrsStore = attrsStore;
    }
  }

  return sub as unknown as Sub;
}

function syncSubToStorage(_k: string, sub: Sub): SubInStorage {
  if (sub.values) {
    const entities: SubEntityInStorage[] = [];
    for (const e of sub.values?.entities) {
      const store = s.toJSON(e.store);
      entities.push({ ...e, store });
    }
    return {
      ...sub,
      values: { attrsStore: sub.values.attrsStore.toJSON(), entities },
    };
  } else {
    return sub as unknown as SubInStorage;
  }
}

function onMergeSub(
  _key: string,
  storageSub: Sub,
  inMemorySub: Sub | null,
): Sub {
  const storageTxId = storageSub?.state?.txId;
  const memoryTxId = inMemorySub?.state?.txId;

  if (storageTxId && (!memoryTxId || storageTxId > memoryTxId)) {
    return storageSub;
  }

  if (memoryTxId && (!storageTxId || memoryTxId > storageTxId)) {
    return inMemorySub;
  }

  return storageSub || inMemorySub;
}

function queryData(sub: Sub, store: s.Store, attrsStore: s.AttrsStore) {
  return instaql(
    { store, attrsStore, pageInfo: null, aggregate: null },
    sub.query,
  ).data;
}

function emptyData(query: any) {
  return Object.fromEntries(
    Object.keys(query)
      .filter((key) => key !== '$$ruleParams')
      .map((key) => [key, []]),
  );
}

function applyChangesToStore(
  store: s.Store,
  attrsStore: s.AttrsStore,
  changes: SyncUpdateTriplesMsg['txes'][number]['changes'],
): void {
  for (const { action, triple } of changes) {
    switch (action) {
      case 'added':
        s.addTriple(store, attrsStore, triple);
        break;
      case 'removed':
        s.retractTriple(store, attrsStore, triple);
        break;
    }
  }
}

function subData(sub: Sub, entities: NonNullable<Sub['values']>['entities']) {
  return entities[0]?.entity || emptyData(sub.query);
}

type CreateStore = (triples: Triple[]) => s.Store;

function sameValue(left: any, right: any) {
  return JSON.stringify(left) === JSON.stringify(right);
}

function changedFields(oldEntity: any, newEntity: any) {
  const changed: Record<string, { oldValue: any; newValue: any }> = {};
  for (const key of new Set([
    ...Object.keys(oldEntity),
    ...Object.keys(newEntity),
  ])) {
    if (!sameValue(oldEntity[key], newEntity[key])) {
      changed[key] = { oldValue: oldEntity[key], newValue: newEntity[key] };
    }
  }
  return changed;
}

function diffData(oldData: any, newData: any) {
  const added: any[] = [];
  const removed: any[] = [];
  const updated: SyncTransaction<any, any, any>['updated'] = [];
  for (const root of new Set([
    ...Object.keys(oldData),
    ...Object.keys(newData),
  ])) {
    const oldEntities = Array.isArray(oldData[root]) ? oldData[root] : [];
    const newEntities = Array.isArray(newData[root]) ? newData[root] : [];
    const oldById = new Map(oldEntities.map((entity) => [entity.id, entity]));
    const newById = new Map(newEntities.map((entity) => [entity.id, entity]));
    for (const [id, entity] of newById) {
      const oldEntity = oldById.get(id);
      if (!oldEntity) {
        added.push(entity);
      } else if (!sameValue(oldEntity, entity)) {
        updated.push({
          oldEntity,
          newEntity: entity,
          changedFields: changedFields(oldEntity, entity),
        });
      }
    }
    for (const [id, entity] of oldById) {
      if (!newById.has(id)) {
        removed.push(entity);
      }
    }
  }
  return { added, removed, updated };
}

export enum CallbackEventType {
  InitialSyncBatch = 'InitialSyncBatch',
  InitialSyncComplete = 'InitialSyncComplete',
  LoadFromStorage = 'LoadFromStorage',
  SyncTransaction = 'SyncTransaction',
  Error = 'Error',
}

type QueryEntities<
  Schema extends IContainEntitiesAndLinks<EntitiesDef, any>,
  Q extends ValidQuery<Q, Schema>,
  UseDates extends boolean,
> = InstaQLResponse<Schema, Q, UseDates>[keyof InstaQLResponse<
  Schema,
  Q,
  UseDates
>];

type QueryEntity<
  Schema extends IContainEntitiesAndLinks<EntitiesDef, any>,
  Q extends ValidQuery<Q, Schema>,
  UseDates extends boolean,
> = QueryEntities<Schema, Q, UseDates> extends (infer E)[] ? E : never;

type ChangedFields<Entity> = {
  [K in keyof Entity]?: {
    oldValue: Entity[K];
    newValue: Entity[K];
  };
};

export interface BaseCallbackEvent<
  Schema extends IContainEntitiesAndLinks<EntitiesDef, any>,
  Q extends ValidQuery<Q, Schema>,
  UseDates extends boolean,
> {
  type: CallbackEventType;
  data: InstaQLResponse<Schema, Q, UseDates>;
}

export interface InitialSyncBatch<
  Schema extends IContainEntitiesAndLinks<EntitiesDef, any>,
  Q extends ValidQuery<Q, Schema>,
  UseDates extends boolean,
> extends BaseCallbackEvent<Schema, Q, UseDates> {
  type: CallbackEventType.InitialSyncBatch;
  batch: QueryEntities<Schema, Q, UseDates>;
}

export interface InitialSyncComplete<
  Schema extends IContainEntitiesAndLinks<EntitiesDef, any>,
  Q extends ValidQuery<Q, Schema>,
  UseDates extends boolean,
> extends BaseCallbackEvent<Schema, Q, UseDates> {
  type: CallbackEventType.InitialSyncComplete;
}

export interface SyncTransaction<
  Schema extends IContainEntitiesAndLinks<EntitiesDef, any>,
  Q extends ValidQuery<Q, Schema>,
  UseDates extends boolean,
> extends BaseCallbackEvent<Schema, Q, UseDates> {
  type: CallbackEventType.SyncTransaction;
  added: QueryEntities<Schema, Q, UseDates>;
  removed: QueryEntities<Schema, Q, UseDates>;
  updated: {
    oldEntity: QueryEntity<Schema, Q, UseDates>;
    newEntity: QueryEntity<Schema, Q, UseDates>;
    changedFields: ChangedFields<QueryEntity<Schema, Q, UseDates>>;
  }[];
}

export interface LoadFromStorage<
  Schema extends IContainEntitiesAndLinks<EntitiesDef, any>,
  Q extends ValidQuery<Q, Schema>,
  UseDates extends boolean,
> extends BaseCallbackEvent<Schema, Q, UseDates> {
  type: CallbackEventType.LoadFromStorage;
}

export interface SetupError<
  Schema extends IContainEntitiesAndLinks<EntitiesDef, any>,
  Q extends ValidQuery<Q, Schema>,
  UseDates extends boolean,
> extends BaseCallbackEvent<Schema, Q, UseDates> {
  type: CallbackEventType.Error;
  error: { message: string; hint?: any; type: string; status: number };
}

export type CallbackEvent<
  Schema extends IContainEntitiesAndLinks<EntitiesDef, any>,
  Q extends ValidQuery<Q, Schema>,
  UseDates extends boolean,
> =
  | InitialSyncBatch<Schema, Q, UseDates>
  | InitialSyncComplete<Schema, Q, UseDates>
  | SyncTransaction<Schema, Q, UseDates>
  | LoadFromStorage<Schema, Q, UseDates>
  | SetupError<Schema, Q, UseDates>;

export type SyncTableCallback<
  Schema extends IContainEntitiesAndLinks<EntitiesDef, any>,
  Q extends ValidQuery<Q, Schema>,
  UseDates extends boolean,
> = (event: CallbackEvent<Schema, Q, UseDates>) => void;

export class SyncTable {
  private trySend: TrySend;
  private subs: PersistedObject<string, Sub, SubInStorage>;
  // Using any for the SyncCallback because we'd need Reactor to be typed
  private callbacks: { [hash: string]: SyncTableCallback<any, any, any>[] } =
    {};
  private config: Config;
  private idToHash: { [subscriptionId: string]: string } = {};
  private log: Logger;
  private createStore: CreateStore;
  private getAttrs: () => s.AttrsStore;

  constructor(
    trySend: TrySend,
    storage: StoreInterface,
    config: Config,
    log: Logger,
    createStore: CreateStore,
    getAttrs: () => s.AttrsStore,
  ) {
    this.trySend = trySend;
    this.config = config;
    this.log = log;
    this.createStore = createStore;
    this.getAttrs = getAttrs;

    this.subs = new PersistedObject<string, Sub, SubInStorage>({
      persister: storage,
      merge: onMergeSub,
      serialize: syncSubToStorage,
      parse: (_key, x) => syncSubFromStorage(x, this.config.useDateObjects),
      objectSize: (sub) => sub.values?.entities.length || 0,
      logger: log,
      gc: {
        maxAgeMs: 1000 * 60 * 60 * 24 * 7 * 52, // 1 year
        maxEntries: 1000,
        // Size of each sub is the number of entity
        maxSize: 1_000_000, // 1 million entities
      },
    });
  }

  public beforeUnload() {
    this.subs.flush();
  }

  public subscribe(
    q: any,
    cb: SyncTableCallback<any, any, any>,
  ): (
    opts?: { keepSubscription?: boolean | null | undefined } | null | undefined,
  ) => void {
    const hash = weakHash(q);
    const callbacks = (this.callbacks[hash] = this.callbacks[hash] || []);
    const isFirstSubscriber = callbacks.length === 0;
    callbacks.push(cb);

    if (isFirstSubscriber) {
      this.initSubscription(q, hash);
    } else {
      this.notifyCurrentSubscription(hash, cb);
    }

    return (opts?: { keepSubscription?: boolean | null | undefined }) => {
      this.unsubscribe(hash, cb, opts?.keepSubscription);
    };
  }

  private unsubscribe(
    hash: string,
    cb: SyncTableCallback<any, any, any>,
    keepSubscription: boolean | null | undefined,
  ) {
    const cbs = (this.callbacks[hash] || []).filter((x) => x !== cb);
    this.callbacks[hash] = cbs;

    if (!cbs.length) {
      delete this.callbacks[hash];
      const sub = this.subs.currentValue[hash];
      if (sub?.state) {
        this.clearSubscriptionData(
          sub.state.subscriptionId,
          !!keepSubscription,
        );
      }
      if (!keepSubscription) {
        this.subs.updateInPlace((prev) => {
          delete prev[hash];
        });
      }
    }
  }

  private sendStart(eventId: string, query: string) {
    this.trySend(eventId, {
      op: 'start-sync',
      q: query,
    });
  }

  private sendRemoveById(subscriptionId: string, keepSubscription: boolean) {
    this.trySend(uuid(), {
      op: 'remove-sync',
      'subscription-id': subscriptionId,
      'keep-subscription': keepSubscription,
    });
  }

  private sendRemove(state: SubState, keepSubscription: boolean) {
    this.trySend(uuid(), {
      op: 'remove-sync',
      'subscription-id': state.subscriptionId,
      'keep-subscription': keepSubscription,
    });
  }

  private async initSubscription(query: any, hash: string) {
    // Wait for storage to load so that we know if we already have an existing subscription
    await this.subs.waitForKeyToLoad(hash);
    const existingSub = this.subs.currentValue[hash];

    if (existingSub?.state) {
      // Re-authorize before exposing persisted data. A server restart or an
      // auth change must not resume a snapshot that is no longer visible.
      delete this.idToHash[existingSub.state.subscriptionId];
      this.sendRemove(existingSub.state, false);
    }

    const startEventId = uuid();
    this.subs.updateInPlace((prev) => {
      prev[hash] = {
        query,
        hash: hash,
        startEventId,
        createdAt: Date.now(),
        updatedAt: Date.now(),
      };
    });

    this.sendStart(startEventId, query);
  }

  private async notifyCurrentSubscription(
    hash: string,
    cb: SyncTableCallback<any, any, any>,
  ) {
    await this.subs.waitForKeyToLoad(hash);
    const sub = this.subs.currentValue[hash];
    if (sub?.state?.txId !== undefined && sub.values?.entities) {
      cb({
        type: CallbackEventType.LoadFromStorage,
        data: subData(sub, sub.values.entities),
      });
    }
  }

  public async flushPending() {
    for (const hash of Object.keys(this.callbacks)) {
      await this.subs.waitForKeyToLoad(hash);
      const sub = this.subs.currentValue[hash];
      if (sub) {
        await this.initSubscription(sub.query, sub.hash);
      } else {
        this.log.error('Missing sub for hash in flushPending', hash);
      }
    }
  }

  public onStartSyncOk(msg: StartSyncOkMsg) {
    const subscriptionId = msg['subscription-id'];
    const q = msg.q;
    const hash = weakHash(q);

    this.subs.updateInPlace((prev) => {
      const sub = prev[hash];
      if (!sub || sub.startEventId !== msg['client-event-id']) {
        this.log.error(
          'Ignoring stale start-sync-ok for hash',
          hash,
          'subscription-id',
          subscriptionId,
          'query',
          q,
        );
        this.sendRemoveById(subscriptionId, false);
        return prev;
      }

      this.idToHash[subscriptionId] = hash;
      sub.state = {
        subscriptionId: subscriptionId,
        token: msg.token,
      };
    });
  }

  private notifyCbs(hash: string, event: CallbackEvent<any, any, any>) {
    for (const cb of this.callbacks[hash] || []) {
      cb(event);
    }
  }

  public onSyncLoadBatch(msg: SyncLoadBatchMsg) {
    const subscriptionId = msg['subscription-id'];
    const joinRows = msg['join-rows'];
    const hash = this.idToHash[subscriptionId];
    if (!hash) {
      this.log.error('Missing hash for subscription', msg);
      return;
    }

    const sub = this.subs.currentValue[hash];
    if (!sub) {
      this.log.error('Missing sub for hash', hash, msg);
      return;
    }

    const values: SubValues = sub.values ?? {
      entities: [],
      attrsStore: this.getAttrs(),
    };
    sub.values = values;
    const existing = values.entities[0];
    const store = existing?.store || this.createStore([]);
    const oldData = subData(sub, values.entities);
    for (const triple of joinRows.flat()) {
      s.addTriple(store, values.attrsStore, triple);
    }
    const data = queryData(sub, store, values.attrsStore);
    const { added: batch } = diffData(oldData, data);
    values.entities = [{ store, entity: data, serverCreatedAt: 0 }];

    this.subs.updateInPlace((prev) => {
      prev[hash] = sub;
      // Make sure we write a field or mutative won't
      // see the change because sub === prev[hash]
      prev[hash].updatedAt = Date.now();
    });

    if (sub.values) {
      this.notifyCbs(hash, {
        type: CallbackEventType.InitialSyncBatch,
        data: subData(sub, sub.values.entities),
        batch,
      });
    }
  }

  public onSyncInitFinish(msg: SyncInitFinishMsg) {
    const subscriptionId = msg['subscription-id'];
    const hash = this.idToHash[subscriptionId];
    if (!hash) {
      this.log.error('Missing hash for subscription', msg);
      return;
    }
    this.subs.updateInPlace((prev) => {
      const sub = prev[hash];
      if (!sub) {
        this.log.error('Missing sub for hash', hash, msg);
        return;
      }
      const state = sub.state;
      if (!state) {
        this.log.error('Sub never set init, missing result', sub, msg);
        return prev;
      }
      state.txId = msg['tx-id'];
      sub.updatedAt = Date.now();
    });

    const sub = this.subs.currentValue[hash];

    if (sub) {
      this.notifyCbs(hash, {
        type: CallbackEventType.InitialSyncComplete,
        data: subData(sub, sub.values?.entities || []),
      });
    }
  }

  public onSyncUpdateTriples(msg: SyncUpdateTriplesMsg) {
    const subscriptionId = msg['subscription-id'];
    const hash = this.idToHash[subscriptionId];
    if (!hash) {
      this.log.error('Missing hash for subscription', msg);
      return;
    }

    const sub = this.subs.currentValue[hash];
    if (!sub) {
      this.log.error('Missing sub for hash', hash, msg);
      return;
    }

    const state = sub.state;
    if (!state) {
      this.log.error('Missing state for sub', sub, msg);
      return;
    }

    for (const tx of msg.txes) {
      if (state.txId && state.txId >= tx['tx-id']) {
        continue;
      }
      state.txId = tx['tx-id'];
      const values: SubValues = sub.values ?? {
        entities: [],
        attrsStore: this.getAttrs(),
      };
      sub.values = values;
      const store = values.entities[0]?.store || this.createStore([]);
      const oldData = subData(sub, values.entities);
      applyChangesToStore(store, values.attrsStore, tx.changes);
      const data = queryData(sub, store, values.attrsStore);
      const { added, removed, updated } = diffData(oldData, data);
      values.entities = [{ store, entity: data, serverCreatedAt: 0 }];
      this.notifyCbs(hash, {
        type: CallbackEventType.SyncTransaction,
        data,
        added,
        removed,
        updated,
      });
    }
    this.subs.updateInPlace((prev) => {
      prev[hash] = sub;
      // Make sure we write a field or mutative won't
      // see the change because sub === prev[hash]
      prev[hash].updatedAt = Date.now();
    });
  }

  private clearSubscriptionData(
    subscriptionId: string,
    keepSubscription: boolean,
  ) {
    const hash = this.idToHash[subscriptionId];
    if (hash) {
      delete this.idToHash[subscriptionId];
      const sub = this.subs.currentValue[hash];
      if (sub?.state) {
        this.sendRemove(sub.state, keepSubscription);
      }
      if (keepSubscription) {
        this.subs.unloadKey(hash);
      } else {
        this.subs.updateInPlace((prev) => {
          delete prev[hash];
        });
      }
      if (sub) {
        return sub;
      }
    }
  }

  public onStartSyncError(msg: {
    op: 'error';
    'original-event': StartMsg;
    'client-event-id': string;
    status: number;
    type: string;
    message?: string;
    hint?: any;
  }) {
    const hash = weakHash(msg['original-event']['q']);
    const sub = this.subs.currentValue[hash];
    if (sub?.startEventId !== msg['client-event-id']) {
      return;
    }
    const error = {
      message: msg.message || 'Uh-oh, something went wrong. Ping Joe & Stopa.',
      status: msg.status,
      type: msg.type,
      hint: msg.hint,
    };

    this.notifyCbs(hash, {
      type: CallbackEventType.Error,
      data: emptyData(msg['original-event']['q']),
      error,
    });
  }

  public onResyncError(msg: {
    op: 'error';
    'original-event': ResyncMsg;
    status: number;
    type: string;
  }) {
    // Clear the subscription and start from scrath on any resync error
    // This can happen if the auth changed and we need to refetch with the
    // new auth or if the subscription is too far behind.
    const subscriptionId = msg['original-event']['subscription-id'];
    const removedSub = this.clearSubscriptionData(subscriptionId, false);
    if (removedSub) {
      this.initSubscription(removedSub.query, removedSub.hash);
    }
  }
}
