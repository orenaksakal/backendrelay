'use client';

import '@/lib/sentry';

import { type ComponentType, type ReactNode, useEffect } from 'react';
import { PostHogProvider } from 'posthog-js/react';
import { NuqsAdapter } from 'nuqs/adapters/react';
import { SWRConfig } from 'swr';
import { ErrorBoundary } from '@/components/ErrorBoundary';
import { PostHogIdentify } from '@/components/PostHogIdentify';
import { Button } from '@/components/ui';
import { memoryCacheProvider } from '@/lib/swrCache';
import { readRetryPolicy } from '@/lib/request';
import { useAuthToken } from '@/lib/auth';
import { isDev } from '@/lib/config';
import { BackupDownloadProvider } from '@/components/dash/BackupDownloadDialog';
import { Dev } from '@/components/Dev';
import posthog from '@/lib/posthog';
import {
  patchFirefoxClicks,
  patchNumberInputScroll,
} from '@/lib/patchBrowserEvents';

function Oops() {
  return (
    <div className="flex flex-col items-center justify-center gap-4 p-8">
      <div className="text-4xl">🙈</div>
      <p className="text-2xl font-bold text-gray-600">Oops!</p>
      <p>An unexpected error occurred. We're on it!</p>
      <Button type="link" href="/dash">
        Back to the dash
      </Button>
    </div>
  );
}

type QueryAdapterProps = { children: ReactNode };

export function Providers({
  children,
  QueryAdapter = NuqsAdapter,
}: {
  children: ReactNode;
  QueryAdapter?: ComponentType<QueryAdapterProps>;
}) {
  const token = useAuthToken();
  useEffect(() => {
    patchNumberInputScroll();
    return patchFirefoxClicks();
  }, []);

  return (
    <PostHogProvider client={posthog}>
      <PostHogIdentify />
      <ErrorBoundary renderError={() => <Oops />}>
        <SWRConfig
          key={token ?? 'anonymous'}
          value={{
            provider: memoryCacheProvider,
            ...readRetryPolicy,
          }}
        >
          <QueryAdapter>
            <BackupDownloadProvider>{children}</BackupDownloadProvider>
          </QueryAdapter>
        </SWRConfig>
      </ErrorBoundary>
      {isDev ? <Dev /> : null}
    </PostHogProvider>
  );
}
