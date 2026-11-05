import * as Sentry from '@sentry/react';

const dsn =
  typeof window === 'undefined'
    ? undefined
    : process.env.NEXT_PUBLIC_SENTRY_DSN || window.__instantConfig?.sentryDSN;

// Initialize at module load so errors during the first render are captured.
// Session replay and tracing stay off: the dashboard reports errors only.
if (dsn) {
  Sentry.init({
    dsn,
    tracesSampleRate: 0,
  });
}

export { Sentry };
