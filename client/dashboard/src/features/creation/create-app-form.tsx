import { useMutation } from '@tanstack/react-query';
import { useRouter, type NavigateOptions } from '@tanstack/react-router';
import { Loader2, Plus } from 'lucide-react';
import { useId, useRef, useState } from 'react';
import { Button } from '@/components/ui/button';
import { Field, FieldError, FieldLabel } from '@/components/ui/field';
import { Input } from '@/components/ui/input';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { CreateOrgDialog } from '@/features/creation/create-org-dialog';
import { apiFetch } from '@/lib/api/client';
import { appEndpoints } from '@/lib/api/endpoints';
import type { App } from '@/lib/api/endpoints/schemas';
import {
  appNameProblem,
  canRecoverCreatedApp,
  createdApp,
  creationErrorMessage,
  creationTargets,
  prepareAppCreation,
  targetForWorkspace,
  type AppCreationDraft,
} from '@/lib/creation/creation';
import { toastManager } from '@/lib/toast';
import {
  APP_LOOKUP_TIMEOUT_MS,
  useBootstrap,
  workspaceQueryKeys,
} from '@/lib/workspace/queries';

const CREATE_APP_ERROR = "Couldn't create the app. Try again.";

export interface CreateAppFormProps {
  /** `?workspace=` from `/new`; ignored when the org is not available. */
  initialWorkspaceId?: string;
  /** `/new` offers the org dialog; first run offers only the app form. */
  showCreateOrg?: boolean;
}

/**
 * C.3 app creation. The browser generates the app and admin-token IDs before
 * the request, so one lookup after a failed response can tell us whether the
 * create succeeded instead of creating a duplicate.
 */
export function CreateAppForm({
  initialWorkspaceId,
  showCreateOrg = false,
}: CreateAppFormProps) {
  const router = useRouter();
  const bootstrap = useBootstrap();
  const targets = creationTargets(bootstrap.data?.orgs);
  const [name, setName] = useState('');
  const [targetId, setTargetId] = useState(
    initialWorkspaceId ?? targetForWorkspace(targets, undefined).id,
  );
  const [nameError, setNameError] = useState<string>();
  const [orgOpen, setOrgOpen] = useState(false);
  const nameRef = useRef<HTMLInputElement>(null);

  const nameId = useId();
  const nameErrorId = `${nameId}-error`;
  const workspaceId = useId();

  const selectedTarget = targetForWorkspace(targets, targetId);

  const createApp = useMutation({
    mutationFn: (draft: AppCreationDraft) => createApplication(draft),
    meta: { invalidates: [workspaceQueryKeys.bootstrap] },
  });

  function submit(): void {
    if (createApp.isPending) return;
    const problem = appNameProblem(name);
    if (problem !== undefined) {
      setNameError(problem);
      nameRef.current?.focus();
      return;
    }
    setNameError(undefined);
    const draft = prepareAppCreation(name, selectedTarget);
    createApp.mutate(draft, {
      onSuccess: (app) => {
        toastManager.add({ type: 'success', title: 'App created' });
        void router.navigate({
          href: `/apps/${encodeURIComponent(app.id)}/overview`,
        } as NavigateOptions);
      },
    });
  }

  return (
    <>
      <section className="border-border bg-card rounded-lg border p-4">
        <form
          className="flex flex-col gap-4"
          aria-label="Create app"
          onSubmit={(event) => {
            event.preventDefault();
            submit();
          }}
        >
          <Field invalid={nameError !== undefined}>
            <FieldLabel htmlFor={nameId}>App name</FieldLabel>
            <Input
              id={nameId}
              ref={nameRef}
              // eslint-disable-next-line jsx-a11y/no-autofocus -- the first field of a creation form takes focus
              autoFocus
              value={name}
              autoComplete="off"
              disabled={createApp.isPending}
              aria-invalid={nameError !== undefined}
              aria-describedby={
                nameError === undefined ? undefined : nameErrorId
              }
              onChange={(event) => {
                setName(event.target.value);
                setNameError(undefined);
                createApp.reset();
              }}
            />
            {nameError === undefined ? null : (
              <FieldError id={nameErrorId} match role="alert">
                {nameError}
              </FieldError>
            )}
          </Field>

          <Field>
            <FieldLabel htmlFor={workspaceId}>Workspace</FieldLabel>
            <Select
              value={selectedTarget.id}
              disabled={createApp.isPending}
              onValueChange={(value) => {
                if (typeof value !== 'string') return;
                setTargetId(value);
                createApp.reset();
              }}
            >
              <SelectTrigger id={workspaceId}>
                <SelectValue>{selectedTarget.title}</SelectValue>
              </SelectTrigger>
              <SelectContent>
                {targets.map((target) => (
                  <SelectItem key={target.id} value={target.id}>
                    {target.title}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </Field>

          {createApp.isError ? (
            <p role="alert" className="text-destructive text-sm">
              {creationErrorMessage(createApp.error, CREATE_APP_ERROR)}
            </p>
          ) : null}

          <div className="flex flex-wrap items-center gap-2">
            <Button
              type="submit"
              disabled={createApp.isPending || name.trim() === ''}
              aria-busy={createApp.isPending}
            >
              {createApp.isPending ? (
                <Loader2 className="animate-spin" aria-hidden="true" />
              ) : (
                <Plus aria-hidden="true" />
              )}
              Create app
            </Button>
            {showCreateOrg ? (
              <Button
                type="button"
                variant="link"
                onClick={() => {
                  setOrgOpen(true);
                }}
              >
                Create an org instead
              </Button>
            ) : null}
          </div>
        </form>
      </section>

      {showCreateOrg ? (
        <CreateOrgDialog open={orgOpen} onOpenChange={setOrgOpen} />
      ) : null}
    </>
  );
}

/**
 * Create, then recover once when the response was lost in transit. A 404 from
 * the lookup means the write did not commit, so the original network error is
 * what the operator sees.
 */
async function createApplication(draft: AppCreationDraft): Promise<App> {
  try {
    const response = await apiFetch(appEndpoints.create, {
      body: {
        id: draft.id,
        admin_token: draft.admin_token,
        title: draft.title,
        ...(draft.org_id === undefined ? {} : { org_id: draft.org_id }),
      },
    });
    return createdApp(response.app, draft);
  } catch (error) {
    if (!canRecoverCreatedApp(error)) throw error;
    const found = await apiFetch(appEndpoints.get, {
      params: { appId: draft.id },
      timeoutMs: APP_LOOKUP_TIMEOUT_MS,
    }).catch(() => undefined);
    if (found === undefined) throw error;
    return createdApp(found.app, draft);
  }
}
