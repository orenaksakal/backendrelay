/**
 * The consent screen's scope wording, ported from the old `/platform/oauth/start`
 * page (C.2). Each entry applies when the granted scope set includes a scope it
 * covers; `apps-write` implies both read and create.
 */
export interface OAuthScopeDescription {
  id: string;
  description: string;
  applies: (scopes: readonly string[]) => boolean;
}

export const OAUTH_SCOPE_DESCRIPTIONS: readonly OAuthScopeDescription[] = [
  {
    id: 'apps-read',
    description:
      'Read access to all of your BackendRelay apps, including schema and permission rules.',
    applies: (scopes) =>
      scopes.includes('apps-read') && !scopes.includes('apps-write'),
  },
  {
    id: 'apps-write',
    description:
      'Read and write access to all of your BackendRelay apps, including schema and permission rules.',
    applies: (scopes) => scopes.includes('apps-write'),
  },
  {
    id: 'apps-create',
    description: 'Create new apps in your BackendRelay account.',
    applies: (scopes) => scopes.includes('apps-write'),
  },
  {
    id: 'data-read',
    description: 'Query all of the data in any of your BackendRelay databases.',
    applies: (scopes) =>
      scopes.includes('data-read') || scopes.includes('data-write'),
  },
  {
    id: 'data-write',
    description:
      'Create, update, and delete data in any of your BackendRelay databases.',
    applies: (scopes) => scopes.includes('data-write'),
  },
  {
    id: 'storage-read',
    description:
      'Download storage ($files) data from any of your BackendRelay databases.',
    applies: (scopes) =>
      scopes.includes('storage-read') || scopes.includes('storage-write'),
  },
  {
    id: 'storage-write',
    description:
      'Upload, update, and delete storage ($files) data from any of your BackendRelay databases.',
    applies: (scopes) => scopes.includes('storage-write'),
  },
];

/** The descriptions that apply to the scopes an app asked for. */
export function consentScopeDescriptions(
  scopes: readonly string[],
): OAuthScopeDescription[] {
  return OAUTH_SCOPE_DESCRIPTIONS.filter((item) => item.applies(scopes));
}
