import { describe, expect, it } from 'vitest';
import {
  historyMenuRuns,
  newRunId,
  PLAYGROUND_HISTORY_LIMIT,
  playgroundHistoryKey,
  readPlaygroundHistory,
  recordPlaygroundRun,
  savePlaygroundRunName,
  trimHistory,
  writePlaygroundHistory,
  type PlaygroundRun,
} from './history';

class MemoryStorage implements Storage {
  private readonly map = new Map<string, string>();

  get length(): number {
    return this.map.size;
  }

  clear(): void {
    this.map.clear();
  }

  getItem(key: string): string | null {
    return this.map.get(key) ?? null;
  }

  key(index: number): string | null {
    return [...this.map.keys()][index] ?? null;
  }

  removeItem(key: string): void {
    this.map.delete(key);
  }

  setItem(key: string, value: string): void {
    this.map.set(key, value);
  }
}

function run(
  index: number,
  overrides: Partial<PlaygroundRun> = {},
): PlaygroundRun {
  return {
    id: `run-${String(index)}`,
    mode: 'query',
    actor: 'admin',
    actorLabel: 'Admin',
    source: `{"todos": ${String(index)}}`,
    createdAt: `2026-10-27T12:${String(index % 60).padStart(2, '0')}:00Z`,
    ...overrides,
  };
}

describe('playground history', () => {
  it('keys the history per app', () => {
    expect(playgroundHistoryKey('app-1')).toBe('br.playground.app-1');
  });

  it('round-trips runs and ignores corrupt storage', () => {
    const storage = new MemoryStorage();
    writePlaygroundHistory('app-1', [run(1)], storage);
    expect(readPlaygroundHistory('app-1', storage)).toEqual([run(1)]);
    expect(readPlaygroundHistory('app-2', storage)).toEqual([]);

    storage.setItem('br.playground.app-1', '{not-json');
    expect(readPlaygroundHistory('app-1', storage)).toEqual([]);
    storage.setItem('br.playground.app-1', JSON.stringify([{ nope: true }]));
    expect(readPlaygroundHistory('app-1', storage)).toEqual([]);
  });

  it('keeps the newest 50 runs', () => {
    let runs: PlaygroundRun[] = [];
    for (let index = 0; index < 60; index += 1) {
      runs = recordPlaygroundRun(runs, run(index)).runs;
    }
    expect(runs).toHaveLength(PLAYGROUND_HISTORY_LIMIT);
    expect(runs[0]?.id).toBe('run-59');
    expect(runs.at(-1)?.id).toBe('run-10');
    expect(trimHistory(runs)).toHaveLength(PLAYGROUND_HISTORY_LIMIT);
  });

  it('does not duplicate a rerun of the same document', () => {
    const first = recordPlaygroundRun([], run(1));
    const again = recordPlaygroundRun(first.runs, {
      ...run(1),
      id: 'run-2',
      createdAt: '2026-10-27T13:00:00Z',
    });
    expect(again.runs).toHaveLength(1);
    expect(again.entry.id).toBe(first.entry.id);
    expect(again.runs[0]?.createdAt).toBe('2026-10-27T13:00:00Z');
  });

  it('pins saved runs above the recent list and never evicts them', () => {
    let runs: PlaygroundRun[] = recordPlaygroundRun([], run(0)).runs;
    const savedId = runs[0]?.id ?? '';
    runs = savePlaygroundRunName(runs, savedId, '  Daily check  ');
    expect(runs[0]?.name).toBe('Daily check');

    for (let index = 1; index < 60; index += 1) {
      runs = recordPlaygroundRun(runs, run(index)).runs;
    }

    expect(runs).toHaveLength(PLAYGROUND_HISTORY_LIMIT);
    expect(runs[0]?.id).toBe(savedId);
    expect(runs[0]?.name).toBe('Daily check');
    expect(historyMenuRuns(runs)[0]?.name).toBe('Daily check');
  });

  it('ignores an empty save name and makes stable ids', () => {
    const runs = [run(1)];
    expect(savePlaygroundRunName(runs, 'run-1', '   ')).toEqual(runs);
    expect(newRunId(1000, () => 0.5)).toMatch(/^rs-[a-z0-9]{4}$/);
    expect(newRunId(1000, () => 0)).toBe(`${(1000).toString(36)}-0000`);
  });
});
