import { expect, test } from 'vitest';
import { MutationStorage } from '../../../src/utils/MutationStorage';
import { META_KEY, StoreInterface } from '../../../src/utils/PersistedObject';

function fixture() {
  const data = new Map<string, Map<string, any>>();
  let fail = false;
  class Storage extends StoreInterface {
    values: Map<string, any>;
    constructor(app: string, store: any) {
      super(app, store);
      const id = `${app}:${store}`;
      if (!data.has(id)) data.set(id, new Map());
      this.values = data.get(id)!;
    }
    async getItem(k: string) {
      return this.values.get(k) ?? null;
    }
    async getAllKeys() {
      return [...this.values.keys()];
    }
    async removeItem(k: string) {
      this.values.delete(k);
    }
    async multiSet(entries: Array<[string, any]>) {
      for (const [k, v] of entries) {
        this.values.set(k, structuredClone(v));
        if (fail) throw new Error('interrupted write');
      }
    }
  }
  return {
    Storage,
    setFailure: (v: boolean) => {
      fail = v;
    },
  };
}

test('custom stores migrate legacy mutations before load and never restore deleted entries', async () => {
  const { Storage } = fixture();
  const kv = new Storage('app', 'kv');
  const event = { op: 'transact', 'tx-steps': [], order: 1 };
  await kv.multiSet([['pendingMutations', JSON.stringify([['event', event]])]]);
  const first = new MutationStorage('app', Storage);
  const second = new MutationStorage('app', Storage);
  expect(await first.getItem('event')).toEqual(event);
  expect(await second.getItem(META_KEY)).toMatchObject({
    objects: { event: { size: 0 } },
  });
  expect(await kv.getItem('pendingMutations')).toBeNull();
  await first.removeItem('event');
  expect(await new MutationStorage('app', Storage).getItem('event')).toBeNull();
});

test('interrupted custom-store migration retains the old queue and retries without overwriting confirmations', async () => {
  const { Storage, setFailure } = fixture();
  const kv = new Storage('app', 'kv');
  const entries = [
    ['a', { order: 1 }],
    ['b', { order: 2 }],
  ];
  await kv.multiSet([['pendingMutations', entries]]);
  setFailure(true);
  await expect(
    new MutationStorage('app', Storage).getAllKeys(),
  ).rejects.toThrow('interrupted');
  expect(await kv.getItem('pendingMutations')).toEqual(entries);
  setFailure(false);
  await new Storage('app', 'mutations').multiSet([
    ['a', { order: 1, 'tx-id': 42 }],
  ]);
  const retried = new MutationStorage('app', Storage);
  expect(await retried.getItem('a')).toEqual({ order: 1, 'tx-id': 42 });
  expect(await retried.getItem('b')).toEqual({ order: 2 });
  expect(await kv.getItem('pendingMutations')).toBeNull();
});
