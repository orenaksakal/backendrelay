import { expect, test, vi } from 'vitest';

vi.mock('react-native-mmkv', () => ({
  createMMKV: () => {
    const values = new Map<string, string>();
    return {
      getString: (key: string) => values.get(key),
      set: (key: string, value: string) => values.set(key, value),
      remove: (key: string) => values.delete(key),
      getAllKeys: () => [...values.keys()],
    };
  },
}));

import Store from './index';

test('serializes values through MMKV storage', async () => {
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
