// Explicit allowlist: never replace process.env wholesale in browser bundles.
export function publicEnv(env) {
  const keys = [
    'BACKEND_URL',
    'STAGING',
    'SELF_HOSTED',
    'LOCAL_SERVER_PORT',
    'RECIPE_API_URI',
    'RECIPE_WEBSOCKET_URI',
    'FALLBACK_STAR_COUNT',
    'FEEDBACK_APP_ID',
    'HOMEPAGE_APP_ID',
    'POSTHOG_KEY',
    'POSTHOG_DEBUG',
    'FEEDBACK_API_URI',
    'SENTRY_DSN',
  ];
  return Object.fromEntries(
    keys.map((key) => [
      `process.env.NEXT_PUBLIC_${key}`,
      JSON.stringify(env[`PUBLIC_${key}`] ?? env[`NEXT_PUBLIC_${key}`]) ??
        'undefined',
    ]),
  );
}
