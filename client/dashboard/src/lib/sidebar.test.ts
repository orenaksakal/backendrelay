import { describe, expect, it } from 'vitest';
import {
  SIDEBAR_STORAGE_KEY,
  readSidebarState,
  sidebarWidth,
  writeSidebarState,
} from './sidebar';

function memoryStorage(initial: Record<string, string> = {}): Storage {
  const values = new Map(Object.entries(initial));
  return {
    get length() {
      return values.size;
    },
    clear() {
      values.clear();
    },
    getItem(key: string) {
      return values.get(key) ?? null;
    },
    key(index: number) {
      return [...values.keys()][index] ?? null;
    },
    removeItem(key: string) {
      values.delete(key);
    },
    setItem(key: string, value: string) {
      values.set(key, value);
    },
  };
}

describe('sidebar state', () => {
  it('defaults to expanded', () => {
    expect(readSidebarState(memoryStorage())).toBe('expanded');
    expect(readSidebarState(null)).toBe('expanded');
  });

  it('round-trips the collapsed preference', () => {
    const storage = memoryStorage();

    writeSidebarState('collapsed', storage);

    expect(storage.getItem(SIDEBAR_STORAGE_KEY)).toBe('collapsed');
    expect(readSidebarState(storage)).toBe('collapsed');
  });

  it('ignores unknown values', () => {
    expect(
      readSidebarState(memoryStorage({ [SIDEBAR_STORAGE_KEY]: 'wide' })),
    ).toBe('expanded');
  });

  it('uses the widths from B.3', () => {
    expect(sidebarWidth('expanded')).toBe(224);
    expect(sidebarWidth('collapsed')).toBe(56);
  });
});
