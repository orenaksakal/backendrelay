import { isApiError } from '@/lib/api/client';
import type {
  App,
  EffectiveRole,
  OrgSummary,
} from '@/lib/api/endpoints/schemas';
import {
  PERSONAL_WORKSPACE_ID,
  PERSONAL_WORKSPACE_TITLE,
} from '@/lib/workspace/workspaces';

/** C.3 limits: app names are short; org names match the server's cap. */
export const APP_NAME_MAX_LENGTH = 80;
export const ORG_NAME_MAX_LENGTH = 140;

/** `app-member` sits below collaborator, so those orgs cannot create apps. */
const APP_CREATION_ROLES: readonly EffectiveRole[] = [
  'owner',
  'admin',
  'collaborator',
];

export interface CreationTarget {
  id: string;
  title: string;
  kind: 'personal' | 'org';
}

export const PERSONAL_TARGET: CreationTarget = {
  id: PERSONAL_WORKSPACE_ID,
  title: PERSONAL_WORKSPACE_TITLE,
  kind: 'personal',
};

/** Personal first, then every org whose role allows creating an app. */
export function creationTargets(
  orgs: readonly OrgSummary[] | undefined,
): CreationTarget[] {
  return [
    PERSONAL_TARGET,
    ...(orgs ?? [])
      .filter((org) => APP_CREATION_ROLES.includes(org.role))
      .map((org) => ({
        id: org.id,
        title: org.title,
        kind: 'org' as const,
      })),
  ];
}

/** Resolves `?workspace=` to an available target, falling back to Personal. */
export function targetForWorkspace(
  targets: readonly CreationTarget[],
  workspaceId: string | undefined,
): CreationTarget {
  return targets.find((target) => target.id === workspaceId) ?? PERSONAL_TARGET;
}

export function appNameProblem(value: string): string | undefined {
  const title = value.trim();
  if (title === '') return 'Enter an app name.';
  if (Array.from(title).length > APP_NAME_MAX_LENGTH) {
    return `App names can contain up to ${String(APP_NAME_MAX_LENGTH)} characters.`;
  }
  return undefined;
}

export function orgNameProblem(value: string): string | undefined {
  const title = value.trim();
  if (title === '') return 'Enter an org name.';
  if (Array.from(title).length > ORG_NAME_MAX_LENGTH) {
    return `Org names can contain up to ${String(ORG_NAME_MAX_LENGTH)} characters.`;
  }
  return undefined;
}

export function isUuid(value: unknown): value is string {
  return (
    typeof value === 'string' &&
    /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(
      value,
    )
  );
}

export interface AppCreationDraft {
  id: string;
  admin_token: string;
  title: string;
  org_id?: string;
}

/**
 * Builds the `POST /dash/apps` body. IDs are generated in the browser so a
 * lost response can be recovered by looking the app up at the same ID instead
 * of creating a duplicate (C.3).
 */
export function prepareAppCreation(
  value: string,
  target: CreationTarget,
  newUuid: () => string = () => crypto.randomUUID(),
): AppCreationDraft {
  const problem = appNameProblem(value);
  if (problem !== undefined) throw new Error(problem);
  return {
    id: newUuid(),
    admin_token: newUuid(),
    title: value.trim(),
    ...(target.kind === 'org' ? { org_id: target.id } : {}),
  };
}

/** A round-trip response must describe the app whose IDs we generated. */
export function createdApp(app: App, draft: Pick<AppCreationDraft, 'id'>): App {
  if (app.id !== draft.id) {
    throw new Error(
      'The server returned a different app. Check the apps list before trying again.',
    );
  }
  return app;
}

/** Only transport failures are ambiguous enough to justify the lookup retry. */
export function canRecoverCreatedApp(error: unknown): boolean {
  return (
    isApiError(error) && (error.type === 'network' || error.type === 'timeout')
  );
}

/** Server text when it is useful; the screen's fallback otherwise. */
export function creationErrorMessage(error: unknown, fallback: string): string {
  if (isApiError(error) && error.message.trim() !== '') return error.message;
  if (error instanceof Error && error.message.trim() !== '') {
    return error.message;
  }
  return fallback;
}
