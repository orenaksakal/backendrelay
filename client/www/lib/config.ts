import { oauthCallbackURL } from '@instantdb/platform';

export const isBrowser = typeof window != 'undefined';

export const isDev = process.env.NODE_ENV === 'development';

export const isSelfHosted = process.env.NEXT_PUBLIC_SELF_HOSTED === 'true';

type DashboardConfig = {
  apiURI: string;
  websocketURI: string;
  dashboardApiURI?: string;
};

type RuntimeDashboardConfig = Partial<DashboardConfig> & {
  sentryDSN?: string;
};

const selfHostedConfigError =
  'Self-hosted dashboard requires INSTANT_API_URI or INSTANT_BACKEND_URL to be a valid HTTP(S) URL.';

declare global {
  interface Window {
    __instantConfig?: RuntimeDashboardConfig;
  }
}

function websocketURIFromApiURI(apiURI: string) {
  const url = new URL(apiURI);
  if (url.protocol !== 'http:' && url.protocol !== 'https:') {
    throw new Error('API URI must use HTTP or HTTPS.');
  }
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  url.pathname = '/runtime/session';
  url.search = '';
  url.hash = '';
  return url.toString();
}

export function configFromApiURI(apiURI: string | undefined | null) {
  if (!apiURI) {
    return null;
  }

  return {
    apiURI,
    websocketURI: websocketURIFromApiURI(apiURI),
  };
}

function getRuntimeApiURI() {
  if (isBrowser) {
    return window.__instantConfig?.apiURI;
  }

  return process.env.INSTANT_API_URI || process.env.INSTANT_BACKEND_URL;
}

export function requireSelfHostedConfig(
  apiURI: string | undefined | null,
  dashboardApiURI?: string,
): DashboardConfig {
  try {
    const config = configFromApiURI(apiURI);
    if (config) {
      if (dashboardApiURI !== undefined) {
        const privateAPI = new URL(dashboardApiURI);
        if (
          !['http:', 'https:'].includes(privateAPI.protocol) ||
          privateAPI.username ||
          privateAPI.password ||
          privateAPI.search ||
          privateAPI.hash
        ) {
          throw new Error('Invalid dashboard API URL');
        }
        return {
          ...config,
          dashboardApiURI: privateAPI.toString().replace(/\/$/, ''),
        };
      }
      return config;
    }
  } catch (_e) {
    // Throw the same actionable error for missing and malformed URLs.
  }
  throw new Error(selfHostedConfigError);
}

const devBackend = getLocal('devBackend');

let localPort = process.env.NEXT_PUBLIC_LOCAL_SERVER_PORT || '8888';

if (devBackend && isBrowser) {
  const portOverride = new URL(location.href).searchParams.get('port');
  if (portOverride) {
    localPort = portOverride;
  }
}

const configuredApiURI = process.env.NEXT_PUBLIC_BACKEND_URL;
const defaultApiURI = configuredApiURI || `http://localhost:${localPort}`;

function getSelfHostedConfig() {
  const apiURI = getRuntimeApiURI() || configuredApiURI;
  const dashboardApiURI = isBrowser
    ? window.__instantConfig?.dashboardApiURI
    : process.env.INSTANT_DASHBOARD_API_URI;
  if (apiURI || isBrowser) {
    return requireSelfHostedConfig(apiURI, dashboardApiURI);
  }

  // The build evaluates this module before the container has runtime config.
  // Container startup validates the URL before serving the dashboard.
  return configFromApiURI(`http://localhost:${localPort}`)!;
}

export const config: DashboardConfig =
  isSelfHosted && !devBackend
    ? getSelfHostedConfig()
    : configFromApiURI(defaultApiURI)!;

// Public apiURI/websocketURI remain suitable for client SDKs and copied app code.
export const dashboardApiURI = config.dashboardApiURI ?? config.apiURI;

export const defaultOAuthCallbackURL = oauthCallbackURL(config.apiURI);

export const demoConfig = {
  apiURI: process.env.NEXT_PUBLIC_RECIPE_API_URI ?? config.apiURI,
  websocketURI:
    process.env.NEXT_PUBLIC_RECIPE_WEBSOCKET_URI ?? config.websocketURI,
};

// In dev mode, sync the devBackend flag to a cookie so server components
// can resolve the same apiURI as the client.
if (isDev && isBrowser) {
  if (devBackend) {
    document.cookie = `devBackend=${localPort}; path=/`;
  } else {
    document.cookie = `devBackend=; path=/; max-age=0`;
  }
}

/**
 * Returns the config for server-side callers. In dev mode, reads the
 * devBackend cookie so it resolves the same apiURI as the client.
 */
export async function getServerConfig(request?: Request) {
  if (isDev && !isBrowser && !configuredApiURI) {
    const devBackendCookie = request?.headers
      .get('cookie')
      ?.split(';')
      .map((cookie) => cookie.trim().split('='))
      .find(([name]) => name === 'devBackend')?.[1];
    if (devBackendCookie) {
      const port = devBackendCookie;
      return {
        apiURI: `http://localhost:${port}`,
        websocketURI: `ws://localhost:${port}/runtime/session`,
      };
    }
  }
  return config;
}

export default config;

export const isTouchDevice =
  typeof window !== 'undefined' && 'ontouchstart' in window;

export function getLocal<T = any>(k: string): T | null {
  if (!isBrowser) {
    return null;
  }

  try {
    const raw = localStorage.getItem(k);

    return raw ? JSON.parse(raw) : null;
  } catch (e) {
    return null;
  }
}

export function setLocal<T = any>(k: string, v: T) {
  if (!isBrowser) {
    return;
  }

  try {
    localStorage.setItem(k, JSON.stringify(v));
  } catch (e) {
    return;
  }
}

export const localStorageFlagPrefix = `__instant__flag__`;

export const cliOauthParamName = '_cli_oauth_ticket';

export const instantRepo = 'instantdb/instant';

export const discordInviteUrl = 'https://discord.com/invite/VU53p7uQcE';

export const discordOAuthAppsFeedbackInviteUrl =
  'https://discord.gg/GrvbPTBDEX';

export const bugsAndQuestionsInviteUrl = 'https://discord.gg/unA5vyV6mP';
