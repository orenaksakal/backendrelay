let startPromise: Promise<void> | undefined;

/**
 * Starts the MSW browser worker in mock mode. `msw/vite` supplies
 * `/mockServiceWorker.js`; outside mock mode the dynamic import is dropped, so
 * production never ships the handlers or the worker.
 */
export function enableMockApi(): Promise<void> {
  if (import.meta.env.VITE_MOCK_API !== '1') return Promise.resolve();

  startPromise ??= import('./browser').then(async ({ worker }) => {
    await worker.start({ onUnhandledFrame: 'bypass' });
  });

  return startPromise;
}
