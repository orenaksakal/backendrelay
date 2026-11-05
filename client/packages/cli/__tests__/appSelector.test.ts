import { describe, expect, test } from 'vitest';
import { UI } from '../src/ui/index.ts';

describe('AppSelector', () => {
  test('returns the selected app after resolving it', () => {
    const selector = new UI.AppSelector({
      allowEphemeral: false,
      allowCreate: false,
      api: {
        getDash: () => ({
          apps: [
            {
              id: 'app-id',
              title: 'App',
              admin_token: 'admin-token',
              created_at: '2026-01-01T00:00:00.000Z',
              magic_code_email_template: null,
            },
          ],
          orgs: [],
        }),
        createEphemeralApp: async () => ({
          appId: 'ephemeral-app-id',
          adminToken: 'ephemeral-admin-token',
        }),
        getAppsForOrg: async () => ({ apps: [] }),
        createApp: async () => ({
          appId: 'new-app-id',
          adminToken: 'new-admin-token',
        }),
      },
    });

    selector.appList.items[0]?.onSelect();

    expect(selector.result()).toEqual({
      appId: 'app-id',
      adminToken: 'admin-token',
      approach: 'import',
    });
  });
});
