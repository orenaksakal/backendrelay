import { describe, expect, it } from 'vitest';
import { connectSnippets, SDK_PACKAGES } from './sdk';

const details = {
  appId: 'app-1',
  apiUrl: 'https://api.example.com',
  websocketUrl: 'wss://api.example.com/runtime/session',
  dashboardUrl: 'https://dash.example.com',
};

describe('connect snippets', () => {
  it('uses the BackendRelay package scope', () => {
    expect(SDK_PACKAGES.react).toBe('@backendrelay/react');
    expect(SDK_PACKAGES.reactNative).toBe('@backendrelay/react-native');
    expect(SDK_PACKAGES.admin).toBe('@backendrelay/admin');
    expect(SDK_PACKAGES.cli).toBe('@backendrelay/cli');
  });

  it('fills the app ID and endpoints into the client snippets', () => {
    const snippets = connectSnippets(details);

    expect(snippets.react.install).toContain('@backendrelay/react');
    expect(snippets.react.code).toContain("appId: 'app-1'");
    expect(snippets.react.code).toContain("apiURI: 'https://api.example.com'");
    expect(snippets.react.code).toContain(
      "websocketURI: 'wss://api.example.com/runtime/session'",
    );
    expect(snippets['react-native'].code).toContain(
      '@backendrelay/react-native',
    );
    expect(snippets.admin.code).toContain(
      'adminToken: process.env.INSTANT_APP_ADMIN_TOKEN!',
    );
  });

  it('prints the CLI environment the CLI reads', () => {
    const { cli } = connectSnippets(details);

    expect(cli.install).toBeUndefined();
    expect(cli.code).toContain('INSTANT_CLI_API_URI=https://api.example.com');
    expect(cli.code).toContain('INSTANT_CLI_DASH_URI=https://dash.example.com');
    expect(cli.code).toContain('npx @backendrelay/cli init --app app-1');
  });
});
