import { expect, test, vi } from 'vitest';

vi.mock('expo-sqlite/kv-store', () => ({
  SQLiteStorage: class {
    private values = new Map<string, string>();

    async getItemAsync(key: string) {
      return this.values.get(key) ?? null;
    }

    async setItemAsync(key: string, value: string) {
      this.values.set(key, value);
    }

    async multiSet(entries: Array<[string, string]>) {
      for (const [key, value] of entries) this.values.set(key, value);
    }

    async removeItemAsync(key: string) {
      this.values.delete(key);
    }

    async getAllKeysAsync() {
      return [...this.values.keys()];
    }
  },
}));

import Store from './index';

test('serializes values through Expo SQLite storage', async () => {
  const store = new Store('app-id', 'kv');

  await store.setItem('one', { nested: true });
  await store.multiSet([
    ['two', ['a', 'b']],
    ['three', null],
  ]);

  await expect(store.getItem('one')).resolves.toEqual({ nested: true });
  await expect(store.getItem('two')).resolves.toEqual(['a', 'b']);
  await expect(store.getItem('missing')).resolves.toBeNull();
  await expect(store.getAllKeys()).resolves.toEqual(['one', 'two', 'three']);

  await store.removeItem('two');
  await expect(store.getAllKeys()).resolves.toEqual(['one', 'three']);
});
