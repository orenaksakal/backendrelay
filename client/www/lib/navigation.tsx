import {
  createContext,
  forwardRef,
  useCallback,
  useContext,
  useMemo,
  useRef,
  useSyncExternalStore,
  type AnchorHTMLAttributes,
  type MouseEvent,
  type ReactNode,
} from 'react';

export type Query = Record<string, string | string[] | undefined>;
export type Href = string | { pathname?: string; query?: Query; hash?: string };
export type NavigationOptions = {
  replace?: boolean;
  shallow?: boolean;
  scroll?: boolean;
};

type NavigationContextValue = {
  location: { pathname: string; search: string; hash?: string };
  navigate: (href: string, options?: NavigationOptions) => void | Promise<void>;
  params?: Record<string, string>;
};

const NavigationContext = createContext<NavigationContextValue | null>(null);

// Keep this list aligned with src/routes.tsx and consume it from site/middleware.ts.
const startRoutePrefixes = [
  '/dash',
  '/_devtool',
  '/platform/oauth/start',
  '/intern/deployment-settings',
];

export function isStartOwnedPathname(pathname: string) {
  return startRoutePrefixes.some(
    (prefix) => pathname === prefix || pathname.startsWith(`${prefix}/`),
  );
}

export function isStartOwnedHref(href: string, origin: string) {
  try {
    const url = new URL(href, origin);
    return url.origin === origin && isStartOwnedPathname(url.pathname);
  } catch {
    return false;
  }
}

export function shouldUseShallowHistory(
  href: string,
  currentHref: string,
  options?: NavigationOptions,
) {
  if (!options?.shallow) return false;
  try {
    const target = new URL(href, currentHref);
    return target.pathname === new URL(currentHref).pathname;
  } catch {
    return false;
  }
}

export function hrefToString(href: Href, pathname = '') {
  if (typeof href === 'string') return href;

  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(href.query ?? {})) {
    if (value === undefined) continue;
    for (const item of Array.isArray(value) ? value : [value])
      search.append(key, item);
  }
  const query = search.toString();
  const hash = href.hash ? `#${href.hash.replace(/^#/, '')}` : '';
  return `${href.pathname ?? pathname}${query ? `?${query}` : ''}${hash}`;
}

export function queryFromSearch(
  search: string,
  params?: Record<string, string>,
) {
  const query: Query = { ...params };
  for (const [key, value] of new URLSearchParams(search)) {
    const existing = query[key];
    query[key] =
      existing === undefined
        ? value
        : [...(Array.isArray(existing) ? existing : [existing]), value];
  }
  for (const [key, value] of Object.entries(params ?? {})) {
    query[key.replace(/[A-Z]/g, (letter) => `-${letter.toLowerCase()}`)] =
      value;
  }
  return query;
}

export function NavigationProvider({
  children,
  location,
  navigate,
  params,
}: NavigationContextValue & { children: ReactNode }) {
  const value = useMemo(
    () => ({ location, navigate, params }),
    [location.pathname, location.search, location.hash, navigate, params],
  );
  return (
    <NavigationContext.Provider value={value}>
      {children}
    </NavigationContext.Provider>
  );
}

type LinkProps = Omit<AnchorHTMLAttributes<HTMLAnchorElement>, 'href'> & {
  href: Href;
  shallow?: boolean;
  replace?: boolean;
  scroll?: boolean;
  prefetch?: boolean;
};

const Link = forwardRef<HTMLAnchorElement, LinkProps>(function Link(
  {
    href,
    replace,
    shallow: _shallow,
    scroll: _scroll,
    prefetch: _prefetch,
    onClick,
    ...props
  },
  ref,
) {
  const navigation = useContext(NavigationContext);
  const hrefString = hrefToString(
    href,
    navigation?.location.pathname ??
      (typeof window === 'undefined' ? '' : window.location.pathname),
  );

  function handleClick(event: MouseEvent<HTMLAnchorElement>) {
    onClick?.(event);
    if (
      event.defaultPrevented ||
      !navigation ||
      event.button !== 0 ||
      event.metaKey ||
      event.altKey ||
      event.ctrlKey ||
      event.shiftKey ||
      props.target ||
      props.download ||
      typeof window === 'undefined' ||
      !isStartOwnedHref(hrefString, window.location.origin)
    ) {
      return;
    }

    event.preventDefault();
    void navigation.navigate(hrefString, { replace });
  }

  return <a {...props} ref={ref} href={hrefString} onClick={handleClick} />;
});

export default Link;

const subscribe = (listener: () => void) => {
  window.addEventListener('popstate', listener);
  window.addEventListener('hashchange', listener);
  return () => {
    window.removeEventListener('popstate', listener);
    window.removeEventListener('hashchange', listener);
  };
};

const getBrowserLocation = () =>
  typeof window === 'undefined'
    ? ''
    : `${window.location.pathname}${window.location.search}${window.location.hash}`;
const getServerLocation = () => '';

function useBrowserLocation() {
  return useSyncExternalStore(subscribe, getBrowserLocation, getServerLocation);
}

function optionsFrom(
  as: Href | NavigationOptions | undefined,
  options: NavigationOptions | undefined,
): NavigationOptions | undefined {
  if (options) return options;
  return as &&
    typeof as === 'object' &&
    !('pathname' in as || 'query' in as || 'hash' in as)
    ? (as as NavigationOptions)
    : undefined;
}

export function useRouter() {
  const navigation = useContext(NavigationContext);
  const browserLocation = useBrowserLocation();
  const navigationRef = useRef(navigation);
  navigationRef.current = navigation;
  const fallbackLocation = useMemo(() => {
    const url = new URL(browserLocation || '/', 'http://localhost');
    return { pathname: url.pathname, search: url.search, hash: url.hash };
  }, [browserLocation]);
  const location = navigation?.location ?? fallbackLocation;

  const navigate = useCallback((href: Href, options?: NavigationOptions) => {
    const activeNavigation = navigationRef.current;
    if (typeof window === 'undefined') return;
    const pathname =
      activeNavigation?.location.pathname ?? window.location.pathname;
    const destination = hrefToString(href, pathname);
    if (
      activeNavigation &&
      isStartOwnedHref(destination, window.location.origin)
    ) {
      return activeNavigation.navigate(destination, options);
    }
    const target = new URL(destination, window.location.href);
    if (shouldUseShallowHistory(destination, window.location.href, options)) {
      window.history[options?.replace ? 'replaceState' : 'pushState'](
        {},
        '',
        target,
      );
      window.dispatchEvent(new PopStateEvent('popstate'));
      return;
    }
    window.location[options?.replace ? 'replace' : 'assign'](target);
  }, []);

  const push = useCallback(
    (url: Href, as?: Href | NavigationOptions, options?: NavigationOptions) =>
      navigate(url, optionsFrom(as, options)),
    [navigate],
  );
  const replace = useCallback(
    (url: Href, as?: Href | NavigationOptions, options?: NavigationOptions) =>
      navigate(url, { ...optionsFrom(as, options), replace: true }),
    [navigate],
  );
  const back = useCallback(() => {
    if (typeof window !== 'undefined') window.history.back();
  }, []);

  return useMemo(
    () => ({
      query: queryFromSearch(location.search, navigation?.params),
      push,
      replace,
      back,
      pathname: location.pathname,
      asPath: `${location.pathname}${location.search}${location.hash ?? ''}`,
      isReady: navigation ? true : browserLocation !== '',
    }),
    [
      back,
      browserLocation,
      location.pathname,
      location.search,
      location.hash,
      navigation?.params,
      navigation !== null,
      push,
      replace,
    ],
  );
}

export function usePathname() {
  return useRouter().pathname;
}

export function useSearchParams() {
  const navigation = useContext(NavigationContext);
  const browserLocation = useBrowserLocation();
  return useMemo(
    () =>
      new URLSearchParams(
        navigation?.location.search ??
          new URL(browserLocation || '/', 'http://localhost').search,
      ),
    [browserLocation, navigation?.location.search],
  );
}

export function useParams() {
  return useContext(NavigationContext)?.params ?? {};
}
