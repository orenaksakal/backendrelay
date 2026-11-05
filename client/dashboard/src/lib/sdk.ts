/**
 * The SDK package names the Connect sheet prints (C.5). They follow the
 * `@backendrelay/*` scope chosen for the fork (PLAN P6.1) and are kept here so a
 * later rename touches one file.
 */
export const SDK_PACKAGES = {
  core: '@backendrelay/core',
  react: '@backendrelay/react',
  reactNative: '@backendrelay/react-native',
  admin: '@backendrelay/admin',
  cli: '@backendrelay/cli',
} as const;

export type ConnectTabId = 'react' | 'react-native' | 'admin' | 'cli';

export interface ConnectDetails {
  appId: string;
  apiUrl: string;
  websocketUrl: string;
  /** The dashboard origin the CLI should talk to. */
  dashboardUrl: string;
}

export interface ConnectSnippet {
  /** Install command; the CLI tab installs nothing because `npx` fetches it. */
  install?: string;
  language: string;
  code: string;
}

export type ConnectSnippets = Record<ConnectTabId, ConnectSnippet>;

function initSnippet(packageName: string, body: ReadonlyArray<string>): string {
  return [
    `import { init } from '${packageName}';`,
    '',
    'const db = init({',
    ...body.map((line) => `  ${line}`),
    '});',
  ].join('\n');
}

/** Ready-to-paste snippets with the app's real IDs and endpoints (C.5). */
export function connectSnippets(details: ConnectDetails): ConnectSnippets {
  const clientConfig = [
    `appId: '${details.appId}',`,
    `apiURI: '${details.apiUrl}',`,
    `websocketURI: '${details.websocketUrl}',`,
  ];

  return {
    react: {
      install: `npm install ${SDK_PACKAGES.react}`,
      language: 'ts',
      code: initSnippet(SDK_PACKAGES.react, clientConfig),
    },
    'react-native': {
      install: `npm install ${SDK_PACKAGES.reactNative}`,
      language: 'ts',
      code: initSnippet(SDK_PACKAGES.reactNative, clientConfig),
    },
    admin: {
      install: `npm install ${SDK_PACKAGES.admin}`,
      language: 'ts',
      code: initSnippet(SDK_PACKAGES.admin, [
        `appId: '${details.appId}',`,
        'adminToken: process.env.INSTANT_APP_ADMIN_TOKEN!,',
        `apiURI: '${details.apiUrl}',`,
      ]),
    },
    cli: {
      language: 'bash',
      code: [
        `INSTANT_CLI_API_URI=${details.apiUrl} \\`,
        `INSTANT_CLI_DASH_URI=${details.dashboardUrl} \\`,
        `npx ${SDK_PACKAGES.cli} init --app ${details.appId}`,
      ].join('\n'),
    },
  };
}
