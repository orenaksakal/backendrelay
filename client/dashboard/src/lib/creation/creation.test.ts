import { describe, expect, it } from 'vitest';
import { ApiError } from '@/lib/api/client';
import type { OrgSummary } from '@/lib/api/endpoints/schemas';
import {
  APP_NAME_MAX_LENGTH,
  ORG_NAME_MAX_LENGTH,
  appNameProblem,
  canRecoverCreatedApp,
  createdApp,
  creationTargets,
  isUuid,
  orgNameProblem,
  prepareAppCreation,
  targetForWorkspace,
} from './creation';

const ORG_ID = '11111111-1111-4111-8111-111111111111';
const APP_ID = '22222222-2222-4222-8222-222222222222';
const ADMIN_TOKEN = '33333333-3333-4333-8333-333333333333';

function org(overrides: Partial<OrgSummary> = {}): OrgSummary {
  return {
    id: ORG_ID,
    title: 'Acme',
    created_at: '2026-01-01T00:00:00Z',
    role: 'owner',
    paid: false,
    ...overrides,
  };
}

describe('creationTargets', () => {
  it('offers Personal first and orgs where the role can create apps', () => {
    const targets = creationTargets([
      org(),
      org({ id: 'admin-org', title: 'Admin org', role: 'admin' }),
      org({
        id: 'collaborator-org',
        title: 'Collaborator org',
        role: 'collaborator',
      }),
    ]);

    expect(targets.map((target) => target.id)).toEqual([
      'personal',
      ORG_ID,
      'admin-org',
      'collaborator-org',
    ]);
    expect(targets[0]).toMatchObject({ title: 'Personal', kind: 'personal' });
  });

  it('hides orgs where the user is only an app member', () => {
    const targets = creationTargets([
      org({ role: 'app-member', title: 'Member only' }),
    ]);

    expect(targets.map((target) => target.title)).toEqual(['Personal']);
  });

  it('handles a payload without orgs', () => {
    expect(creationTargets(undefined)).toHaveLength(1);
  });
});

describe('targetForWorkspace', () => {
  const targets = creationTargets([org()]);

  it('resolves an available org', () => {
    expect(targetForWorkspace(targets, ORG_ID).id).toBe(ORG_ID);
  });

  it('falls back to Personal for a missing or unknown workspace', () => {
    expect(targetForWorkspace(targets, undefined).id).toBe('personal');
    expect(targetForWorkspace(targets, 'deleted-org').id).toBe('personal');
  });
});

describe('name validation', () => {
  it('trims app names and rejects empty or overlong input', () => {
    expect(appNameProblem('  todo-app  ')).toBeUndefined();
    expect(appNameProblem('   ')).toBe('Enter an app name.');
    expect(appNameProblem('a'.repeat(APP_NAME_MAX_LENGTH))).toBeUndefined();
    expect(appNameProblem('a'.repeat(APP_NAME_MAX_LENGTH + 1))).toBe(
      `App names can contain up to ${String(APP_NAME_MAX_LENGTH)} characters.`,
    );
  });

  it('counts org names by characters, not bytes', () => {
    expect(orgNameProblem('Acme')).toBeUndefined();
    expect(orgNameProblem('é'.repeat(ORG_NAME_MAX_LENGTH))).toBeUndefined();
    expect(orgNameProblem('é'.repeat(ORG_NAME_MAX_LENGTH + 1))).toBe(
      `Org names can contain up to ${String(ORG_NAME_MAX_LENGTH)} characters.`,
    );
  });
});

describe('prepareAppCreation', () => {
  const newUuid = () => APP_ID;

  it('puts a personal app in no org', () => {
    const draft = prepareAppCreation(
      '  todo-app  ',
      { id: 'personal', title: 'Personal', kind: 'personal' },
      newUuid,
    );

    expect(draft).toEqual({
      id: APP_ID,
      admin_token: APP_ID,
      title: 'todo-app',
    });
  });

  it('includes org_id when the workspace is an org', () => {
    const draft = prepareAppCreation(
      'todo-app',
      { id: ORG_ID, title: 'Acme', kind: 'org' },
      newUuid,
    );

    expect(draft).toMatchObject({ title: 'todo-app', org_id: ORG_ID });
  });

  it('refuses an invalid name before generating credentials', () => {
    expect(() =>
      prepareAppCreation(
        '   ',
        { id: 'personal', title: 'Personal', kind: 'personal' },
        newUuid,
      ),
    ).toThrow('Enter an app name.');
  });
});

describe('createdApp', () => {
  const app = {
    id: APP_ID,
    title: 'todo-app',
    created_at: '2026-01-01T00:00:00Z',
  };

  it('accepts the app whose ID we generated', () => {
    expect(createdApp(app, { id: APP_ID }).id).toBe(APP_ID);
  });

  it('rejects a response for a different app', () => {
    expect(() => createdApp(app, { id: ADMIN_TOKEN })).toThrow(/different app/);
  });
});

describe('isUuid', () => {
  it('matches the IDs generated for apps and admin tokens', () => {
    expect(isUuid(APP_ID)).toBe(true);
    expect(isUuid('not-a-uuid')).toBe(false);
    expect(isUuid(undefined)).toBe(false);
  });
});

describe('canRecoverCreatedApp', () => {
  it('retries only ambiguous transport failures', () => {
    expect(
      canRecoverCreatedApp(
        new ApiError({
          status: 0,
          type: 'network',
          message: 'Could not reach the server.',
        }),
      ),
    ).toBe(true);
    expect(
      canRecoverCreatedApp(
        new ApiError({
          status: 0,
          type: 'timeout',
          message: 'The request timed out.',
        }),
      ),
    ).toBe(true);
    expect(
      canRecoverCreatedApp(
        new ApiError({
          status: 400,
          type: 'validation',
          message: 'Malformed title',
        }),
      ),
    ).toBe(false);
    expect(canRecoverCreatedApp(new Error('network'))).toBe(false);
  });
});
