import { useEffect, useMemo } from 'react';
import { useSWRConfig } from 'swr';

type Scope = { consumers: number; controllers: Set<AbortController> };
const scopes = new WeakMap<object, Map<string, Scope>>();

/** SWR invokes one fetcher for all same-key subscribers, so they share its lifetime. */
export function useReadScope(key: string) {
  const { cache } = useSWRConfig();
  const owner = useMemo(() => {
    let registry = scopes.get(cache);
    if (!registry) {
      registry = new Map();
      scopes.set(cache, registry);
    }
    const get = () => {
      let scope = registry.get(key);
      if (!scope) {
        scope = { consumers: 0, controllers: new Set() };
        registry.set(key, scope);
      }
      return scope;
    };
    return {
      subscribe() {
        const scope = get();
        scope.consumers++;
        return () => {
          scope.consumers--;
          // StrictMode unmounts and immediately remounts effects during development.
          queueMicrotask(() => {
            if (scope.consumers === 0) {
              for (const controller of scope.controllers) controller.abort();
              if (registry.get(key) === scope) registry.delete(key);
            }
          });
        };
      },
      async run<T>(read: (signal: AbortSignal) => Promise<T>) {
        const scope = get();
        const controller = new AbortController();
        scope.controllers.add(controller);
        try {
          return await read(controller.signal);
        } finally {
          scope.controllers.delete(controller);
        }
      },
    };
  }, [cache, key]);
  useEffect(() => owner.subscribe(), [owner]);
  return owner.run;
}
