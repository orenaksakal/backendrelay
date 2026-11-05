import { useCallback, useEffect, useRef, useSyncExternalStore } from 'react';

function getSnapshot<T>(k: string): T | undefined {
  if (typeof window == 'undefined') return;
  try {
    const v = window.localStorage.getItem(k);
    if (!v) return;
    return JSON.parse(v);
  } catch (e) {}
}

function setItem<T>(k: string, v: T | undefined) {
  if (typeof window == 'undefined') {
    throw new Error('useLocalStorage/setState needs to run on the client');
  }
  const stringified = JSON.stringify(v);

  try {
    if (stringified === undefined) window.localStorage.removeItem(k);
    else window.localStorage.setItem(k, stringified);
  } catch (e) {
    // Keep the preference usable in memory when persistence is unavailable.
  }
  // localStorage.setItem does not dispatch events to the current
  window.dispatchEvent(
    new StorageEvent('storage', { key: k, newValue: stringified }),
  );
}

export default function useLocalStorage<T>(
  k: string,
  defaultValue: T,
  saveDefaultValue = false,
): [T, (v: T | undefined) => void] {
  const snapshotRef = useRef<T>(getSnapshot<T>(k) ?? defaultValue);
  const keyRef = useRef(k);
  if (keyRef.current !== k) {
    keyRef.current = k;
    snapshotRef.current = getSnapshot<T>(k) ?? defaultValue;
  }
  const subscribe = useCallback(
    (cb: Function) => {
      const listener = (e: StorageEvent) => {
        if (e.key !== null && e.key !== k) return;
        try {
          snapshotRef.current =
            e.newValue == null ? defaultValue : JSON.parse(e.newValue);
        } catch {
          snapshotRef.current = defaultValue;
        }
        cb();
      };
      window.addEventListener('storage', listener);
      return () => {
        window.removeEventListener('storage', listener);
      };
    },
    [k, defaultValue],
  );
  const state = useSyncExternalStore<T>(
    subscribe,
    () => snapshotRef.current,
    () => defaultValue,
  );

  useEffect(() => {
    if (saveDefaultValue && defaultValue == snapshotRef.current) {
      setItem<T>(k, defaultValue);
    }
  }, [k, defaultValue, saveDefaultValue]);

  return [state, (v: T | undefined) => setItem<T>(k, v)];
}
