import { useCallback, useState } from 'react';

/**
 * Sidebar width preference, remembered across tabs and visits (B.3). The value
 * is the collapsed/expanded state, not a pixel width, so the design can change
 * without rewriting old preferences.
 */
export const SIDEBAR_STORAGE_KEY = 'br.sidebar';

export type SidebarState = 'expanded' | 'collapsed';

export const SIDEBAR_EXPANDED_WIDTH = 224;
export const SIDEBAR_COLLAPSED_WIDTH = 56;

export function sidebarWidth(state: SidebarState): number {
  return state === 'collapsed'
    ? SIDEBAR_COLLAPSED_WIDTH
    : SIDEBAR_EXPANDED_WIDTH;
}

export function readSidebarState(
  storage: Storage | null = browserStorage(),
): SidebarState {
  if (storage === null) return 'expanded';
  try {
    return storage.getItem(SIDEBAR_STORAGE_KEY) === 'collapsed'
      ? 'collapsed'
      : 'expanded';
  } catch {
    return 'expanded';
  }
}

export function writeSidebarState(
  state: SidebarState,
  storage: Storage | null = browserStorage(),
): void {
  if (storage === null) return;
  try {
    storage.setItem(SIDEBAR_STORAGE_KEY, state);
  } catch {
    /* The shell still collapses for this session when storage is blocked. */
  }
}

/** Collapse state plus a setter that persists the preference. */
export function useSidebarState(): [
  SidebarState,
  (next: SidebarState) => void,
] {
  const [state, setState] = useState<SidebarState>(() => readSidebarState());

  const update = useCallback((next: SidebarState) => {
    setState(next);
    writeSidebarState(next);
  }, []);

  return [state, update];
}

function browserStorage(): Storage | null {
  if (typeof window === 'undefined') return null;
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}
