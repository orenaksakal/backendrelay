import {
  DashboardRequestError,
  isRetryableRead,
  requestError,
  type Request,
} from '@/lib/request';
import { parseActivity, type Activity } from './activity';
export type PollUpdate = {
  loading?: boolean;
  activity?: Activity | null;
  error?: string;
  failures?: number;
  received?: string;
};
export function startActivityPolling({
  path,
  request,
  intervalMs,
  onUpdate,
  initialFetch = true,
  isVisible = () => true,
  schedule = (fn, ms) => setTimeout(fn, ms),
  cancel = (id) => clearTimeout(id as ReturnType<typeof setTimeout>),
}: {
  path: string;
  request: Request;
  intervalMs: number;
  onUpdate: (update: PollUpdate) => void;
  initialFetch?: boolean;
  isVisible?: () => boolean;
  schedule?: (fn: () => void, ms: number) => unknown;
  cancel?: (id: unknown) => void;
}) {
  let stopped = false,
    pending = false,
    failures = 0,
    retryAllowed = true,
    timer: unknown,
    timeout: unknown,
    controller: AbortController | null = null;
  const clear = () => {
    if (timer !== undefined) cancel(timer);
    timer = undefined;
  };
  async function run() {
    if (stopped || pending || !isVisible()) return;
    clear();
    pending = true;
    controller = new AbortController();
    const abort = controller;
    let timedOut = false;
    timeout = schedule(() => {
      timedOut = true;
      abort.abort();
    }, 15000);
    onUpdate({ loading: true });
    try {
      const result = parseActivity(
        await request(path, { signal: abort.signal }),
      );
      if (stopped || abort.signal.aborted) return;
      failures = 0;
      retryAllowed = true;
      onUpdate({
        activity: result,
        error: '',
        failures: 0,
        received: new Date().toISOString(),
      });
    } catch (e) {
      if (!stopped && (!abort.signal.aborted || timedOut)) {
        failures++;
        retryAllowed = timedOut || isRetryableRead(e);
        const denied =
          e instanceof DashboardRequestError && [401, 403].includes(e.status);
        onUpdate({
          ...(denied ? { activity: null, received: undefined } : {}),
          error: timedOut ? 'Activity request timed out.' : requestError(e),
          failures,
        });
      }
    } finally {
      if (timeout !== undefined) cancel(timeout);
      timeout = undefined;
      pending = false;
      if (!stopped) {
        onUpdate({ loading: false });
        if (intervalMs > 0 && retryAllowed && failures < 3 && isVisible())
          timer = schedule(() => void run(), intervalMs);
      }
    }
  }
  if (initialFetch) void run();
  return {
    refresh() {
      failures = 0;
      retryAllowed = true;
      void run();
    },
    visibilityChanged() {
      clear();
      if (!isVisible()) {
        controller?.abort();
      } else if (intervalMs > 0 && retryAllowed && !pending && failures < 3)
        void run();
    },
    stop() {
      stopped = true;
      clear();
      if (timeout !== undefined) cancel(timeout);
      controller?.abort();
    },
  };
}
