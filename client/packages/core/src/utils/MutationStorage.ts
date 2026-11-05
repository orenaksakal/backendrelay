import {
  META_KEY,
  StoreInterface,
  type StoreInterfaceClass,
  type Meta,
} from './PersistedObject.ts';

// IndexedDB migrates transactionally when opening its database. Other SDK
// stores (AsyncStorage, MMKV, SQLite and custom stores) need the same upgrade.
// Gate every access until the copy is durable and the legacy queue is removed.
// If a write fails, the original queue remains available for the next open.
const migrations = new WeakMap<
  StoreInterfaceClass,
  Map<string, Promise<void>>
>();

export class MutationStorage extends StoreInterface {
  private readonly store: StoreInterface;
  private readonly ready: Promise<void>;

  constructor(appId: string, Storage: StoreInterfaceClass) {
    super(appId, 'mutations');
    this.store = new Storage(appId, 'mutations');
    let apps = migrations.get(Storage);
    if (!apps) migrations.set(Storage, (apps = new Map()));
    let pending = apps.get(appId);
    if (!pending) {
      pending = this.migrate(new Storage(appId, 'kv'));
      apps.set(appId, pending);
      const clear = () => {
        if (apps.get(appId) === pending) apps.delete(appId);
      };
      void pending.then(clear, clear);
    }
    this.ready = pending;
  }

  private async migrate(kv: StoreInterface) {
    const legacy = await kv.getItem('pendingMutations');
    if (legacy == null) return;
    const entries = typeof legacy === 'string' ? JSON.parse(legacy) : legacy;
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
    )
      throw new Error('Invalid persisted pending mutations');
    const meta: Meta<string> = (await this.store.getItem(META_KEY)) ?? {
      objects: {},
    };
    const writes: Array<[string, any]> = [];
    for (const [id, mutation] of entries) {
      // A partial prior upgrade may have copied a newer server confirmation.
      if ((await this.store.getItem(id)) == null) writes.push([id, mutation]);
      meta.objects[id] ??= {
        createdAt: Date.now(),
        updatedAt: Date.now(),
        size: 0,
      };
    }
    await this.store.multiSet([...writes, [META_KEY, meta]]);
    await kv.removeItem('pendingMutations');
  }

  async getItem(key: string) {
    await this.ready;
    return this.store.getItem(key);
  }
  async getAllKeys() {
    await this.ready;
    return this.store.getAllKeys();
  }
  async removeItem(key: string) {
    await this.ready;
    await this.store.removeItem(key);
  }
  async multiSet(values: Array<[string, any]>) {
    await this.ready;
    await this.store.multiSet(values);
  }
}
