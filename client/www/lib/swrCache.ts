const caches = new Set<WeakRef<Map<any, any>>>();

/** Authenticated responses live only in memory, including after a reload. */
export function memoryCacheProvider() {
  const cache = new Map();
  caches.add(new WeakRef(cache));
  return cache;
}

export function removeLegacyResponseCaches(storage?: Storage) {
  try {
    storage ??= typeof window === 'undefined' ? undefined : window.localStorage;
    if (!storage) return;
    const keys = Array.from({ length: storage.length }, (_, index) =>
      storage!.key(index),
    );
    for (const key of keys) {
      if (key === 'swr-cache' || key?.startsWith('dash:'))
        storage.removeItem(key);
    }
  } catch {
    // Blocked or unavailable storage must never break authentication.
  }
}

export function clearMemoryCaches() {
  for (const ref of caches) {
    const cache = ref.deref();
    if (cache) cache.clear();
    else caches.delete(ref);
  }
}
