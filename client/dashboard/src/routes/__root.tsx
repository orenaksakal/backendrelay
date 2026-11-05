import {
  HeadContent,
  Outlet,
  Scripts,
  createRootRouteWithContext,
} from '@tanstack/react-router';
import { QueryClientProvider } from '@tanstack/react-query';
import { ThemeProvider } from '@/components/theme-provider';
import { startSession } from '@/lib/auth/session';
import type { RouterContext } from '@/lib/router-context';
import {
  getRuntimeConfigResult,
  loadRuntimeConfig,
} from '@/lib/runtime-config';
import globalStylesheet from '@/styles/globals.css?url';

export const Route = createRootRouteWithContext<RouterContext>()({
  // The container generates /config.json at start, so validate it before any
  // route renders. A broken deployment gets a plain page instead of a broken UI.
  // The router's client `hydrate` hook loads it for the SPA shell too, which
  // reuses the server's root match instead of re-running `beforeLoad`.
  beforeLoad: async () => {
    await loadRuntimeConfig();
    // Migration, cross-tab sync and the 401 handler need the storage origin,
    // which only exists in the browser.
    startSession();
  },
  head: () => ({
    meta: [
      { charSet: 'utf-8' },
      { name: 'viewport', content: 'width=device-width, initial-scale=1' },
      { name: 'application-name', content: 'BackendRelay' },
    ],
    links: [{ rel: 'stylesheet', href: globalStylesheet }],
  }),
  component: RootComponent,
});

function RootComponent() {
  const { queryClient } = Route.useRouteContext();
  const runtimeConfig = getRuntimeConfigResult();

  if (runtimeConfig === undefined) {
    return (
      <html lang="en">
        <head>
          <meta charSet="utf-8" />
          <meta name="viewport" content="width=device-width, initial-scale=1" />
          {/* Blocking, so the theme class exists before the first paint. */}
          <script src="/theme-init.js" />
          <link rel="stylesheet" href={globalStylesheet} />
          <title>Loading · BackendRelay</title>
        </head>
        <body>
          <main id="main">
            <h1>Loading BackendRelay</h1>
            <p>The dashboard configuration has not loaded yet.</p>
          </main>
        </body>
      </html>
    );
  }

  if (!runtimeConfig.ok) {
    return (
      <html lang="en">
        <head>
          <meta charSet="utf-8" />
          <meta name="viewport" content="width=device-width, initial-scale=1" />
          {/* Blocking, so the theme class exists before the first paint. */}
          <script src="/theme-init.js" />
          <link rel="stylesheet" href={globalStylesheet} />
          <title>Configuration error · BackendRelay</title>
        </head>
        <body>
          <main id="main">
            <h1>BackendRelay is not configured</h1>
            <p>The dashboard could not load a valid /config.json.</p>
            <pre>{runtimeConfig.message}</pre>
          </main>
        </body>
      </html>
    );
  }

  return (
    <html lang="en">
      <head>
        {/* Blocking, so the theme class exists before the first paint. */}
        <script src="/theme-init.js" />
        <HeadContent />
      </head>
      <body>
        <QueryClientProvider client={queryClient}>
          <ThemeProvider>
            <Outlet />
          </ThemeProvider>
        </QueryClientProvider>
        <Scripts />
      </body>
    </html>
  );
}
