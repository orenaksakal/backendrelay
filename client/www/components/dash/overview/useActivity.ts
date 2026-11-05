import { useEffect, useRef, useState } from 'react';
import { startActivityPolling, type PollUpdate } from './polling';
import type { Request } from '@/lib/request';
export function useActivity(
  path: string,
  request: Request,
  intervalMs: number,
) {
  const [state, setState] = useState<PollUpdate>({ loading: true }),
    [hidden, setHidden] = useState(false);
  const poller = useRef<ReturnType<typeof startActivityPolling> | null>(null),
    lastPath = useRef(path),
    lastRequest = useRef(request),
    hasData = useRef(false);
  useEffect(() => {
    if (lastPath.current !== path || lastRequest.current !== request) {
      lastPath.current = path;
      lastRequest.current = request;
      hasData.current = false;
      setState({ loading: true });
    }
    setHidden(document.hidden);
    setState((s) => ({ ...s, loading: false }));
    const current = startActivityPolling({
      path,
      request,
      intervalMs,
      initialFetch: intervalMs > 0 || !hasData.current,
      onUpdate: (update) => {
        if (update.activity) hasData.current = true;
        setState((s) => ({ ...s, ...update }));
      },
      isVisible: () => !document.hidden && navigator.onLine !== false,
    });
    poller.current = current;
    const changed = () => {
      setHidden(document.hidden);
      current.visibilityChanged();
    };
    document.addEventListener('visibilitychange', changed);
    window.addEventListener('online', changed);
    window.addEventListener('offline', changed);
    return () => {
      current.stop();
      document.removeEventListener('visibilitychange', changed);
      window.removeEventListener('online', changed);
      window.removeEventListener('offline', changed);
    };
  }, [path, request, intervalMs]);
  const current = lastPath.current === path && lastRequest.current === request;
  return {
    ...(current ? state : { loading: true }),
    hidden,
    refresh: () => poller.current?.refresh(),
  };
}
