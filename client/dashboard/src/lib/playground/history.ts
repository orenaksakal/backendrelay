import type { PlaygroundActorKind, PlaygroundMode } from './url-state';

/**
 * C.12 History: the last 50 runs per app in `localStorage` under
 * `br.playground.<appId>`. Named ("Save as…") runs stay at the top of the menu
 * and are never evicted by the 50-run cap; refresh tokens are never stored, so
 * a Token run restores the editor but asks for the token again.
 */

export const PLAYGROUND_HISTORY_LIMIT = 50;

export interface PlaygroundRun {
  id: string;
  mode: PlaygroundMode;
  actor: PlaygroundActorKind;
  actorLabel: string;
  /** Stored for email runs so the combobox can be pre-filled. */
  email?: string;
  source: string;
  createdAt: string;
  name?: string;
}

export function playgroundHistoryKey(appId: string): string {
  return `br.playground.${appId}`;
}

function storageOrNull(): Storage | null {
  try {
    return typeof window === 'undefined' ? null : window.localStorage;
  } catch {
    return null;
  }
}

function isRun(value: unknown): value is PlaygroundRun {
  if (value === null || typeof value !== 'object') return false;
  const run = value as Record<string, unknown>;
  return (
    typeof run['id'] === 'string' &&
    (run['mode'] === 'query' || run['mode'] === 'transact') &&
    (run['actor'] === 'admin' ||
      run['actor'] === 'guest' ||
      run['actor'] === 'email' ||
      run['actor'] === 'token') &&
    typeof run['actorLabel'] === 'string' &&
    typeof run['source'] === 'string' &&
    typeof run['createdAt'] === 'string' &&
    (run['email'] === undefined || typeof run['email'] === 'string') &&
    (run['name'] === undefined || typeof run['name'] === 'string')
  );
}

/** Named runs first, then the newest unnamed runs, capped at 50. */
export function trimHistory(
  runs: ReadonlyArray<PlaygroundRun>,
): PlaygroundRun[] {
  const named = runs.filter((run) => run.name !== undefined);
  const unnamed = runs.filter((run) => run.name === undefined);
  return [...named, ...unnamed].slice(0, PLAYGROUND_HISTORY_LIMIT);
}

export function readPlaygroundHistory(
  appId: string,
  storage: Storage | null = storageOrNull(),
): PlaygroundRun[] {
  if (storage === null) return [];
  let parsed: unknown;
  try {
    const stored = storage.getItem(playgroundHistoryKey(appId)) ?? '[]';
    parsed = JSON.parse(stored) as unknown;
  } catch {
    return [];
  }
  if (!Array.isArray(parsed)) return [];
  return trimHistory(parsed.filter(isRun));
}

export function writePlaygroundHistory(
  appId: string,
  runs: ReadonlyArray<PlaygroundRun>,
  storage: Storage | null = storageOrNull(),
): void {
  if (storage === null) return;
  try {
    storage.setItem(
      playgroundHistoryKey(appId),
      JSON.stringify(trimHistory(runs)),
    );
  } catch {
    /* A full or blocked localStorage only loses the history. */
  }
}

function sameRun(left: PlaygroundRun, right: PlaygroundRun): boolean {
  return (
    left.mode === right.mode &&
    left.actor === right.actor &&
    (left.email ?? '') === (right.email ?? '') &&
    left.source === right.source
  );
}

/**
 * Records a run. Re-running the same document as the same actor keeps the
 * existing entry (and its name) at the top instead of adding a duplicate.
 */
export function recordPlaygroundRun(
  runs: ReadonlyArray<PlaygroundRun>,
  run: PlaygroundRun,
): { runs: PlaygroundRun[]; entry: PlaygroundRun } {
  const duplicate = runs.find((candidate) => sameRun(candidate, run));
  const entry: PlaygroundRun =
    duplicate === undefined ? run : { ...duplicate, createdAt: run.createdAt };
  const rest = runs.filter((candidate) => candidate.id !== entry.id);
  return { runs: trimHistory([entry, ...rest]), entry };
}

/** "Save as…": names the run and pins it above the recent list. */
export function savePlaygroundRunName(
  runs: ReadonlyArray<PlaygroundRun>,
  id: string,
  name: string,
): PlaygroundRun[] {
  const trimmed = name.trim();
  if (trimmed === '') return [...runs];
  return trimHistory(
    runs.map((run) => (run.id === id ? { ...run, name: trimmed } : run)),
  );
}

/** The History menu order: saved runs first, then recent runs. */
export function historyMenuRuns(
  runs: ReadonlyArray<PlaygroundRun>,
  limit = PLAYGROUND_HISTORY_LIMIT,
): PlaygroundRun[] {
  const named = runs.filter((run) => run.name !== undefined);
  const unnamed = runs.filter((run) => run.name === undefined);
  return [...named, ...unnamed].slice(0, limit);
}

export function newRunId(
  now: number = Date.now(),
  random: () => number = Math.random,
): string {
  const suffix = Math.floor(random() * 1_000_000)
    .toString(36)
    .padStart(4, '0');
  return `${now.toString(36)}-${suffix}`;
}
