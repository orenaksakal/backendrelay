import { useCallback } from 'react';
import { usePostHog } from 'posthog-js/react';
import config, { dashboardApiURI, isSelfHosted } from '@/lib/config';
import { jsonFetch } from '@/lib/fetch';
import useLocalStorage from '@/lib/hooks/useLocalStorage';
import type { InstantApp } from '@/lib/types';
import { Copyable, SectionHeading } from '@/components/ui';
import { HomeButton } from '@/pages/dash';
import { AppStart } from './HomeStartGuide';
import { useActivity } from './overview/useActivity';
import type { Request } from '@/lib/request';

// Original Home composition, retaining the bounded/visibility-aware Rust-port
// activity polling instead of bringing back overlapping setInterval requests.
export function OriginalHome({
  app,
  token,
}: {
  app: InstantApp;
  token: string;
}) {
  const posthog = usePostHog();
  const request: Request = useCallback(
    (path, init) =>
      jsonFetch(`${config.apiURI}${path}`, {
        ...init,
        headers: { ...init?.headers, Authorization: `Bearer ${token}` },
      }),
    [token],
  );
  const { activity, error, loading, refresh } = useActivity(
    `/dash/apps/${app.id}/stats`,
    request,
    5000,
  );
  const [hideAppId, setHideAppId] = useLocalStorage('hide_app_id', false);
  return (
    <div className="max-w-2xl p-4 text-sm md:text-base">
      <div className="pb-10">
        <AppStart
          app={app}
          backend={
            isSelfHosted
              ? {
                  apiURI: config.apiURI,
                  dashURI: window.location.origin,
                  dashboardApiURI,
                }
              : undefined
          }
        />
      </div>
      <SectionHeading>Next Steps</SectionHeading>
      <p className="pt-1">
        Now that you have your app running, here's some helpful links on what to
        do next!
      </p>
      <div className="flex flex-col gap-4 pt-4 md:flex-row">
        <HomeButton
          href="/docs"
          title="Read the Docs"
          target="_blank"
          onClick={() =>
            posthog.capture('getting_started_click', {
              action: 'read_docs',
              app_id: app.id,
            })
          }
        >
          Jump into our docs to start learning how to use Instant.
        </HomeButton>
        <HomeButton href="/docs/init" title="Connect your app" target="_blank">
          Initialize the SDK with your App ID and your backend endpoints.
        </HomeButton>
      </div>
      <div className="mt-10">
        <SectionHeading>Your Public App ID</SectionHeading>
        <p className="pt-1">
          Use this App ID to connect to your database via init. This ID is safe
          to use in public-facing applications.
        </p>
        <div className="mt-4">
          <Copyable
            value={app.id}
            size="large"
            hideValue={hideAppId}
            onChangeHideValue={() => setHideAppId(!hideAppId)}
          />
        </div>
      </div>
      <div className="mt-10">
        <SectionHeading>Your App Statistics</SectionHeading>
        <div
          className="mt-4 space-y-4 rounded-sm border bg-white p-4 dark:border-neutral-700 dark:bg-neutral-800"
          aria-live="polite"
        >
          {error ? (
            <div role="alert">
              <p>{error}</p>
              <button className="mt-2 underline" onClick={refresh}>
                Retry statistics
              </button>
            </div>
          ) : activity ? (
            <p>
              <strong className="text-3xl tabular-nums">
                {activity.count.toLocaleString()}
              </strong>{' '}
              sessions are connected right now
            </p>
          ) : (
            <p>{loading ? 'Loading…' : 'Connection statistics unavailable.'}</p>
          )}
          {!error && activity && Object.keys(activity.origins).length > 0 && (
            <div className="space-y-2 border-t pt-4 dark:border-neutral-700">
              <div className="text-sm text-gray-500 dark:text-neutral-400">
                Per Origin
              </div>
              {Object.entries(activity.origins)
                .sort(([, a], [, b]) => b - a)
                .map(([origin, count]) => (
                  <div key={origin} className="flex justify-between gap-4">
                    <span className="truncate">{origin}</span>
                    <span>{count.toLocaleString()}</span>
                  </div>
                ))}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
