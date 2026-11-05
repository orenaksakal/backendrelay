import type { ReactNode } from 'react';
import {
  HeadContent,
  ClientOnly,
  Outlet,
  Scripts,
  createRootRoute,
  useLocation,
  useNavigate,
  useParams,
} from '@tanstack/react-router';
import { Providers } from '../../app/providers';
import { NuqsAdapter } from 'nuqs/adapters/tanstack-router';
import { NavigationProvider } from '@/lib/navigation';
import { isSelfHosted } from '@/lib/config';
import globalStylesheet from '@/styles/globals.css?url';

declare global {
  function __getAppId(): any;
}

export const Route = createRootRoute({
  head: () => ({
    meta: [
      { charSet: 'utf-8' },
      {
        name: 'viewport',
        content: 'width=device-width, initial-scale=1',
      },
      { name: 'theme-color', content: '#ffffff' },
      { name: 'application-name', content: 'BackendRelay' },
      { property: 'og:type', content: 'website' },
      { property: 'og:site_name', content: 'BackendRelay' },
      { property: 'og:image', content: '/opengraph-image.png' },
      { name: 'twitter:card', content: 'summary_large_image' },
    ],
    links: [
      { rel: 'stylesheet', href: globalStylesheet },
      { rel: 'icon', type: 'image/svg+xml', href: '/relay.svg' },
      { rel: 'apple-touch-icon', href: '/relay.svg' },
      { rel: 'manifest', href: '/site.webmanifest' },
      {
        rel: 'preload',
        href: '/fonts/Switzer-Regular.woff',
        as: 'font',
        type: 'font/woff',
        crossOrigin: 'anonymous',
      },
    ],
  }),
  component: RootComponent,
});

function RootComponent() {
  return (
    <Document>
      <ClientOnly>
        <NavigationDriver>
          <Providers QueryAdapter={NuqsAdapter}>
            <Outlet />
          </Providers>
        </NavigationDriver>
      </ClientOnly>
    </Document>
  );
}

function NavigationDriver({ children }: { children: ReactNode }) {
  const location = useLocation();
  const navigate = useNavigate();
  const params = useParams({ strict: false });

  return (
    <NavigationProvider
      location={{
        pathname: location.pathname,
        search: location.searchStr,
        hash: location.hash,
      }}
      navigate={(href, options) =>
        navigate({
          to: href,
          replace: options?.replace,
          // Standalone pages have separate CSS bundles. A document transition
          // keeps those bundles intact; same-page tabs and filters stay client-side.
          reloadDocument:
            new URL(href, window.location.href).pathname !== location.pathname,
        })
      }
      params={params}
    >
      {children}
    </NavigationProvider>
  );
}

function Document({ children }: { children: ReactNode }) {
  return (
    <html lang="en">
      <head>
        <HeadContent />
      </head>
      <body>
        {isSelfHosted && <script src="/publicSelfHostedVariables.js" />}
        {children}
        <Scripts />
      </body>
    </html>
  );
}
