import { describe, expect, it } from 'vitest';
import {
  DEFAULT_PAGE_SIZE,
  PAGE_SIZE_STORAGE_KEY,
  isPageSize,
  readDefaultPageSize,
  writeDefaultPageSize,
} from './preferences';

function memoryStorage(): Storage {
  const values = new Map<string, string>();
  return {
    get length() {
      return values.size;
    },
    clear: () => {
      values.clear();
    },
    getItem: (key: string) => values.get(key) ?? null,
    key: (index: number) => [...values.keys()][index] ?? null,
    removeItem: (key: string) => {
      values.delete(key);
    },
    setItem: (key: string, value: string) => {
      values.set(key, value);
    },
  };
}

describe('account preferences', () => {
  it('defaults the page size to 50 and rejects unsupported values', () => {
    const storage = memoryStorage();

    expect(readDefaultPageSize(storage)).toBe(DEFAULT_PAGE_SIZE);

    storage.setItem(PAGE_SIZE_STORAGE_KEY, '100');
    expect(readDefaultPageSize(storage)).toBe(100);

    storage.setItem(PAGE_SIZE_STORAGE_KEY, '13');
    expect(readDefaultPageSize(storage)).toBe(DEFAULT_PAGE_SIZE);

    writeDefaultPageSize(200, storage);
    expect(storage.getItem(PAGE_SIZE_STORAGE_KEY)).toBe('200');
  });

  it('accepts only the offered page sizes', () => {
    expect(isPageSize(25)).toBe(true);
    expect(isPageSize(50)).toBe(true);
    expect(isPageSize(200)).toBe(true);
    expect(isPageSize(0)).toBe(false);
    expect(isPageSize(500)).toBe(false);
    expect(isPageSize(Number.NaN)).toBe(false);
  });
});
