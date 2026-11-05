import {
  useMutation,
  useQuery,
  useQueryClient,
  type QueryClient,
} from '@tanstack/react-query';
import { useEffect, useState } from 'react';
import { apiFetch, isApiError } from '@/lib/api/client';
import { appEndpoints } from '@/lib/api/endpoints';
import type { App, Dash } from '@/lib/api/endpoints/schemas';
import {
  appSchemaFromAttrs,
  type AppSchema,
  type InstantAttr,
} from '@/lib/instant';
import { appQueryOptions, workspaceQueryKeys } from '@/lib/workspace/queries';
import type { Rules, RuleVersion } from './model';
import type { DraftValidation } from './validation';

/**
 * C.10 data: the app record that carries the live rules, the version history,
 * the save mutation, the schema that feeds completions, and the debounced D0.5
 * validation. Saving writes the acknowledged document into both cached app
 * shapes so the header and a later history load agree immediately.
 */

export const permissionsQueryKeys = {
  versions: (appId: string) =>
    ['dash', 'apps', appId, 'rule-versions'] as const,
  schema: (appId: string) => ['dash', 'apps', appId, 'schema-pull'] as const,
};

/** D0.5 runs 600ms after typing stops, matching the plan's invalidate window. */
export const RULES_VALIDATION_DEBOUNCE_MS = 600;

/** `GET /dash/apps/:id` has `rules` and `rules_version`; the bootstrap list may not. */
export function useRulesApp(appId: string) {
  return useQuery(appQueryOptions(appId));
}

export function useRuleVersions(appId: string, enabled: boolean) {
  return useQuery({
    queryKey: permissionsQueryKeys.versions(appId),
    queryFn: ({ signal }) =>
      apiFetch(appEndpoints.ruleVersions, { params: { appId }, signal }),
    enabled,
  });
}

/** The endpoint's `edits` array is validated as JSON by the reconstruction. */
export function ruleVersionsFrom(
  versions: ReadonlyArray<{
    version: number;
    edits: unknown;
    created_at: string;
  }>,
): RuleVersion[] {
  return versions.map((entry) => ({
    version: entry.version,
    edits: Array.isArray(entry.edits) ? entry.edits : [],
    created_at: entry.created_at,
  }));
}

function updateCachedRules(
  queryClient: QueryClient,
  appId: string,
  rules: Rules,
  version: number | undefined,
): void {
  const update = (app: App): App => ({
    ...app,
    rules,
    rules_version: version ?? app.rules_version,
  });
  queryClient.setQueryData<{ app: App }>(
    workspaceQueryKeys.app(appId),
    (current) =>
      current === undefined ? current : { app: update(current.app) },
  );
  queryClient.setQueryData<Dash>(workspaceQueryKeys.bootstrap, (current) =>
    current === undefined
      ? current
      : {
          ...current,
          apps: current.apps.map((app) =>
            app.id === appId ? update(app) : app,
          ),
        },
  );
}

export function useSaveRules(appId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (code: Rules) =>
      apiFetch(appEndpoints.saveRules, { params: { appId }, body: { code } }),
    onSuccess: (result, code) => {
      updateCachedRules(queryClient, appId, code, result.rules?.version);
      void queryClient.invalidateQueries({
        queryKey: permissionsQueryKeys.versions(appId),
      });
    },
  });
}

/**
 * The namespace/attribute catalog for the visual editor. Collaborators cannot
 * use the admin-token Instant client, so `schema/pull` (role ≥ C) provides the
 * same public shape; a failure only costs completions, not the editor.
 */
export function usePermissionsSchema(appId: string) {
  return useQuery({
    queryKey: permissionsQueryKeys.schema(appId),
    queryFn: async ({ signal }): Promise<AppSchema> => {
      const result = await apiFetch(appEndpoints.schemaPull, {
        params: { appId },
        signal,
      });
      const attrs: Record<string, InstantAttr> = {};
      for (const attribute of result.attrs) {
        attrs[attribute.id] = attribute as unknown as InstantAttr;
      }
      return appSchemaFromAttrs(attrs);
    },
    staleTime: 60_000,
  });
}

/**
 * Debounced live validation through D0.5. Only dirty drafts are checked; the
 * saved document is already known to compile. Aborting the request when the
 * draft changes keeps late responses from overwriting newer errors.
 */
export function useDraftValidation(
  appId: string,
  code: Rules | null,
  enabled: boolean,
): DraftValidation {
  const [validation, setValidation] = useState<DraftValidation>({
    state: 'idle',
  });
  const serialized = code === null ? null : JSON.stringify(code);

  useEffect(() => {
    if (!enabled || serialized === null) {
      // eslint-disable-next-line react-hooks/set-state-in-effect -- validation is idle when there is no rule text
      setValidation({ state: 'idle' });
      return;
    }
    const controller = new AbortController();
    setValidation({ state: 'checking' });
    const timer = setTimeout(() => {
      void (async () => {
        try {
          const response = await apiFetch(appEndpoints.validateRules, {
            params: { appId },
            body: { code: JSON.parse(serialized) as Rules },
            signal: controller.signal,
          });
          if (controller.signal.aborted) return;
          setValidation(
            response.ok
              ? { state: 'valid' }
              : {
                  state: 'invalid',
                  errors: response.errors,
                },
          );
        } catch (error) {
          if (controller.signal.aborted) return;
          setValidation({
            state: 'invalid',
            errors: [
              {
                in: [],
                message: isApiError(error)
                  ? error.message
                  : 'Could not check these rules. Try again.',
              },
            ],
          });
        }
      })();
    }, RULES_VALIDATION_DEBOUNCE_MS);

    return () => {
      clearTimeout(timer);
      controller.abort();
    };
  }, [appId, enabled, serialized]);

  return validation;
}
