import {
  Meta,
  META_KEY,
  StoreInterface,
  StoreInterfaceStoreName,
} from './utils/PersistedObject.ts';
import InMemoryStore from './InMemoryStorage.ts';

// Access via globalThis so missing `indexedDB` (Safari private browsing,
// embedded webviews, storage disabled) is `undefined` instead of a
// ReferenceError on the free variable.
function getIndexedDB(): IDBFactory | undefined {
  try {
    const idb = globalThis.indexedDB;
    if (idb && typeof idb.open === 'function') {
      return idb;
    }
  } catch {
    // Some environments throw when reading storage APIs.
  }
  return undefined;
}

// Any time these are updates to the data format or new stores are added,
// the version must be updated.
// onupgradeneeded will be called, which is where you can
// move objects from one idb to another.
// We create a new IDB for each version change instead of
// using their built-in versioning because they have no ability
// to roll back and if multiple tabs are active, then you'll just
// be stuck.
const version = 7;

const storeNames = ['kv', 'querySubs', 'syncSubs', 'mutations'] as const;

// Check that we're not missing a store name in storeNames
type MissingStoreNames = Exclude<
  StoreInterfaceStoreName,
  (typeof storeNames)[number]
>;
const _exhaustiveCheck: never = null as MissingStoreNames;

function logErrorCb(source: string) {
  return function logError(event) {
    console.error('Error in IndexedDB event', { source, event });
  };
}

async function existingDb(name: string): Promise<IDBDatabase | null> {
  const idb = getIndexedDB();
  if (!idb) {
    return null;
  }
  return new Promise((resolve) => {
    const request = idb.open(name);

    request.onerror = (_event) => {
      resolve(null);
    };

    request.onsuccess = (event) => {
      const target = event.target as IDBOpenDBRequest;
      const db = target.result;
      resolve(db);
    };

    request.onupgradeneeded = (event) => {
      const target = event.target as IDBOpenDBRequest;
      target.transaction?.abort();
      resolve(null);
    };
  });
}

// Copy the old database without deleting it. A marker committed with every
// copied store makes interruption recoverable and prevents repeating migration
// over newer writes on subsequent opens.
async function upgrade6To7(appId: string, db: IDBDatabase): Promise<void> {
  const marker = '__mutation_migration_v7';
  const read = (
    database: IDBDatabase,
    store: string,
  ): Promise<Array<[IDBValidKey, any]>> =>
    new Promise((resolve, reject) => {
      const tx = database.transaction(store, 'readonly');
      const entries: Array<[IDBValidKey, any]> = [];
      const request = tx.objectStore(store).openCursor();
      request.onsuccess = () => {
        const cursor = request.result;
        if (cursor) {
          entries.push([cursor.key, cursor.value]);
          cursor.continue();
        }
      };
      tx.oncomplete = () => resolve(entries);
      tx.onerror = tx.onabort = () => reject(tx.error);
    });
  if ((await read(db, '_migrations')).some(([key]) => key === marker)) return;
  const old = await existingDb(`instant_${appId}_6`);
  const stores = ['kv', 'querySubs', 'syncSubs'] as const;
  const data = new Map<string, Array<[IDBValidKey, any]>>();
  try {
    if (old) {
      for (const store of stores) data.set(store, await read(old, store));
    } else {
      // Transform v5 entirely in memory; publish only in the final atomic
      // transaction. Another tab may finish migration while these reads run.
      const v5 = await existingDb(`instant_${appId}_5`);
      const kv: Array<[IDBValidKey, any]> = [];
      const queries: Array<[IDBValidKey, any]> = [];
      const kvMeta: Meta<string> = { objects: {} };
      const queryMeta: Meta<string> = { objects: {} };
      try {
        for (const [key, raw] of v5 ? await read(v5, 'kv') : []) {
          if (key === 'querySubs') {
            const subs = typeof raw === 'string' ? JSON.parse(raw) : raw;
            for (const [hash, entry] of Object.entries(subs ?? {})) {
              const value: any =
                typeof entry === 'string' ? JSON.parse(entry) : entry;
              queries.push([hash, value]);
              if (value.lastAccessed)
                queryMeta.objects[hash] = {
                  createdAt: value.lastAccessed,
                  updatedAt: value.lastAccessed,
                  size: value.result?.store?.triples?.length ?? 0,
                };
            }
          } else {
            kv.push([key, raw]);
            kvMeta.objects[String(key)] = {
              createdAt: Date.now(),
              updatedAt: Date.now(),
              size: 0,
            };
          }
        }
      } finally {
        v5?.close();
      }
      data.set('kv', v5 ? [...kv, [META_KEY, kvMeta]] : []);
      data.set('querySubs', v5 ? [...queries, [META_KEY, queryMeta]] : []);
      data.set('syncSubs', []);
    }
  } finally {
    old?.close();
  }
  const kv = new Map(data.get('kv'));
  const blob = kv.get('pendingMutations');
  const entries = typeof blob === 'string' ? JSON.parse(blob) : (blob ?? []);
  if (
    !Array.isArray(entries) ||
    entries.some(
      (entry) =>
        !Array.isArray(entry) ||
        entry.length !== 2 ||
        typeof entry[0] !== 'string' ||
        entry[0] === META_KEY ||
        !entry[1] ||
        typeof entry[1] !== 'object' ||
        Array.isArray(entry[1]),
    )
  ) {
    throw new Error('Invalid persisted pending mutations');
  }
  kv.delete('pendingMutations');
  const oldMeta = kv.get(META_KEY);
  if (oldMeta?.objects) delete oldMeta.objects.pendingMutations;
  data.set('kv', [...kv]);
  const meta: Meta<string> = { objects: {} };
  for (const [id] of entries)
    meta.objects[id] = {
      createdAt: Date.now(),
      updatedAt: Date.now(),
      size: 0,
    };
  await new Promise<void>((resolve, reject) => {
    const tx = db.transaction([...storeNames, '_migrations'], 'readwrite');
    tx.oncomplete = () => resolve();
    tx.onerror = tx.onabort = () => reject(tx.error);
    const check = tx.objectStore('_migrations').get(marker);
    check.onsuccess = () => {
      // The marker check and all writes share the same write lock across tabs.
      if (check.result) return;
      for (const [name, values] of data)
        for (const [key, value] of values) tx.objectStore(name).put(value, key);
      for (const [id, mutation] of entries)
        tx.objectStore('mutations').put(mutation, id);
      tx.objectStore('kv').delete('pendingMutations');
      tx.objectStore('mutations').put(meta, META_KEY);
      tx.objectStore('_migrations').put(true, marker);
      tx.commit?.();
    };
  });
}

// We create many IndexedDBStorage instances that talk to the same
// underlying db, but we only get one `onupgradeneeded` event. This holds
// the upgrade promises so that we wait until upgrade finishes before
// we start writing.
const upgradePromises = new Map();

export default class IndexedDBStorage extends StoreInterface {
  dbName: string;
  _storeName: StoreInterfaceStoreName;
  _appId: string;
  _prefix: string;
  _dbPromise: Promise<IDBDatabase | null>;
  private _fallback: InMemoryStore;

  constructor(appId: string, storeName: StoreInterfaceStoreName) {
    super(appId, storeName);
    this.dbName = `instant_${appId}_${version}`;
    this._storeName = storeName;
    this._appId = appId;
    this._fallback = new InMemoryStore(appId, storeName);
    this._dbPromise = this._init();
  }

  _init(): Promise<IDBDatabase | null> {
    return new Promise<IDBDatabase | null>((resolve, reject) => {
      const idb = getIndexedDB();
      if (!idb) {
        reject(new Error('IndexedDB is not available'));
        return;
      }
      const request = idb.open(this.dbName, 1);

      request.onerror = (event) => {
        reject(event);
      };

      request.onsuccess = (event) => {
        const target = event.target as IDBOpenDBRequest;
        const db = target.result;
        // Browsers can close IndexedDB connections unexpectedly
        // (e.g. backgrounded tabs, memory pressure, version changes from
        // other tabs). Re-init so the next operation gets a fresh connection.
        db.onclose = () => {
          this._dbPromise = this._init();
        };
        db.onversionchange = () => {
          db.close();
        };
        // Publish the barrier synchronously in onsuccess before another
        // connection can start using this database.
        let migration = upgradePromises.get(this.dbName);
        if (!migration) {
          migration = upgrade6To7(this._appId, db);
          upgradePromises.set(this.dbName, migration);
        }
        migration
          .then(() => resolve(db))
          .catch((error) => {
            upgradePromises.delete(this.dbName);
            db.close();
            reject(error);
          });
      };

      request.onupgradeneeded = (event) => {
        this._upgradeStore(event);
      };
    }).catch(() => {
      // Persistence is optional; keep the complete storage interface usable in
      // private/restricted contexts (#2779). Never swallow later write errors.
      return null;
    });
  }

  _upgradeStore(event: IDBVersionChangeEvent) {
    const target = event.target as IDBOpenDBRequest;
    const db = target.result;
    for (const storeName of [...storeNames, '_migrations']) {
      if (!db.objectStoreNames.contains(storeName)) {
        db.createObjectStore(storeName);
      }
    }
  }

  // Browsers can close IndexedDB connections unexpectedly (backgrounded tabs,
  // memory pressure, cross-tab version changes, etc.), causing
  // `db.transaction()` to throw InvalidStateError. This helper catches that
  // error and retries once with a fresh connection.
  async _withRetry<T>(
    fn: (db: IDBDatabase) => Promise<T>,
    fallback: () => Promise<T>,
  ): Promise<T> {
    try {
      const db = await this._dbPromise;
      return db ? await fn(db) : await fallback();
    } catch (e) {
      if (e instanceof DOMException && e.name === 'InvalidStateError') {
        this._dbPromise = this._init();
        const db = await this._dbPromise;
        return db ? await fn(db) : await fallback();
      }
      throw e;
    }
  }

  async getItem(k: string): Promise<any> {
    return this._withRetry(
      (db) => {
        return new Promise((resolve, reject) => {
          const transaction = db.transaction([this._storeName], 'readonly');
          const objectStore = transaction.objectStore(this._storeName);
          const request = objectStore.get(k);
          request.onerror = () => {
            reject(request.error);
          };
          request.onsuccess = () => {
            if (request.result) {
              resolve(request.result);
            } else {
              resolve(null);
            }
          };
        });
      },
      () => this._fallback.getItem(k),
    );
  }

  async setItem(k: string, v: any): Promise<void> {
    return this._withRetry(
      (db) => {
        return new Promise((resolve, reject) => {
          const transaction = db.transaction([this._storeName], 'readwrite');
          const objectStore = transaction.objectStore(this._storeName);
          objectStore.put(v, k);

          transaction.oncomplete = () => resolve();
          transaction.onerror = () => reject(transaction.error);
          transaction.onabort = () => reject(transaction.error);
          transaction.commit?.();
        });
      },
      () => this._fallback.setItem(k, v),
    );
  }

  async multiSet(keyValuePairs: Array<[string, any]>): Promise<void> {
    return this._withRetry(
      (db) => {
        return new Promise((resolve, reject) => {
          const transaction = db.transaction([this._storeName], 'readwrite');
          const objectStore = transaction.objectStore(this._storeName);
          for (const [k, v] of keyValuePairs) {
            objectStore.put(v, k);
          }

          transaction.oncomplete = () => resolve();
          transaction.onerror = () => reject(transaction.error);
          transaction.onabort = () => reject(transaction.error);
          transaction.commit?.();
        });
      },
      () => this._fallback.multiSet(keyValuePairs),
    );
  }

  async removeItem(k: string): Promise<void> {
    return this._withRetry(
      (db) => {
        return new Promise((resolve, reject) => {
          const transaction = db.transaction([this._storeName], 'readwrite');
          const objectStore = transaction.objectStore(this._storeName);
          objectStore.delete(k);

          transaction.oncomplete = () => resolve();
          transaction.onerror = () => reject(transaction.error);
          transaction.onabort = () => reject(transaction.error);
          transaction.commit?.();
        });
      },
      () => this._fallback.removeItem(k),
    );
  }

  async getAllKeys(): Promise<string[]> {
    return this._withRetry(
      (db) => {
        return new Promise((resolve, reject) => {
          const transaction = db.transaction([this._storeName], 'readonly');
          const objectStore = transaction.objectStore(this._storeName);
          const request = objectStore.getAllKeys();
          request.onerror = () => {
            reject(request.error);
          };
          request.onsuccess = () => {
            resolve(request.result.filter((x) => typeof x === 'string'));
          };
        });
      },
      () => this._fallback.getAllKeys(),
    );
  }
}
