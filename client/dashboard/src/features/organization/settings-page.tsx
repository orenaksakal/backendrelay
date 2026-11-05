import { useRouter, type NavigateOptions } from '@tanstack/react-router';
import { Settings } from 'lucide-react';
import { useId, useState } from 'react';
import { EmptyState } from '@/components/feedback/empty-state';
import { ErrorState } from '@/components/feedback/error-state';
import { TableSkeleton } from '@/components/feedback/skeletons';
import { TypedConfirmDialog } from '@/components/feedback/typed-confirm-dialog';
import { PageHeader } from '@/components/layout/page-header';
import { SectionCard } from '@/components/layout/section-card';
import { SettingsRow } from '@/components/layout/settings-row';
import { UnsavedChangesBar } from '@/components/layout/unsaved-changes-bar';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { isApiError } from '@/lib/api/client';
import type { OrgDetails } from '@/lib/api/endpoints/schemas';
import { ORG_NAME_MAX_LENGTH, orgNameProblem } from '@/lib/creation/creation';
import { orgErrorMessage } from '@/lib/organization/errors';
import { canDeleteOrg, canManageOrg } from '@/lib/organization/model';
import { useDeleteOrg, useRenameOrg } from '@/lib/organization/queries';
import { toastManager } from '@/lib/toast';
import { useOrgDetails } from '@/lib/workspace/queries';
import { PERSONAL_WORKSPACE_ID } from '@/lib/workspace/workspaces';

const RENAME_ERROR = "Couldn't rename the org. Try again.";
const DELETE_ERROR = "Couldn't delete the org. Try again.";
const APPS_BLOCK_DELETE = 'Move or delete its apps first';

export interface OrgSettingsPageProps {
  workspaceId: string;
}

/**
 * C.17 settings: rename (admin) and the delete-organization danger zone
 * (owner, blocked while the org still owns apps).
 */
export function OrgSettingsPage({ workspaceId }: OrgSettingsPageProps) {
  const isPersonal = workspaceId === PERSONAL_WORKSPACE_ID;
  const org = useOrgDetails(isPersonal ? null : workspaceId);

  if (isPersonal) {
    return (
      <div className="flex max-w-3xl flex-col gap-4">
        <PageHeader title="Settings" />
        <EmptyState
          icon={Settings}
          title="Personal workspaces have no organization settings"
          description="Organization settings cover the shared name, members and deletion."
        />
      </div>
    );
  }

  if (org.isError) {
    return (
      <div className="flex max-w-3xl flex-col gap-4">
        <PageHeader title="Settings" />
        <ErrorState
          thing="organization"
          status={isApiError(org.error) ? org.error.status : undefined}
          message={org.error instanceof Error ? org.error.message : undefined}
          requestId={isApiError(org.error) ? org.error.requestId : undefined}
          onRetry={() => {
            void org.refetch();
          }}
        />
      </div>
    );
  }

  if (org.data === undefined) {
    return (
      <div className="flex max-w-3xl flex-col gap-4">
        <PageHeader title="Settings" />
        <TableSkeleton rows={2} columns={2} />
      </div>
    );
  }

  return <OrgSettingsContent workspaceId={workspaceId} details={org.data} />;
}

interface OrgSettingsContentProps {
  workspaceId: string;
  details: OrgDetails;
}

function OrgSettingsContent({ workspaceId, details }: OrgSettingsContentProps) {
  const router = useRouter();
  const renameOrg = useRenameOrg(workspaceId);
  const deleteOrg = useDeleteOrg(workspaceId);
  const [draft, setDraft] = useState<string>();
  const [nameError, setNameError] = useState<string>();
  const [deleteOpen, setDeleteOpen] = useState(false);
  const nameId = useId();
  const nameErrorId = `${nameId}-error`;
  const appsBlockId = useId();

  const title = details.org.title;
  const value = draft ?? title;
  const dirty = draft !== undefined && draft !== title;
  const canRename = canManageOrg(details.org.role);
  const canDelete = canDeleteOrg(details.org.role);
  const hasApps = details.apps.length > 0;

  if (details.org.role === 'app-member') {
    return (
      <div className="flex max-w-3xl flex-col gap-4">
        <PageHeader title="Settings" description={title} />
        <ErrorState
          thing="organization settings"
          status={403}
          requiredRole="collaborator"
          message="Only members of this organization can see its settings."
        />
      </div>
    );
  }

  function save(): void {
    if (!dirty || renameOrg.isPending) return;
    const problem = orgNameProblem(value);
    if (problem !== undefined) {
      setNameError(problem);
      return;
    }
    setNameError(undefined);
    renameOrg.mutate(value.trim(), {
      onSuccess: () => {
        setDraft(undefined);
        toastManager.add({ type: 'success', title: 'Org renamed' });
      },
    });
  }

  return (
    <div className="flex max-w-3xl flex-col gap-4">
      <PageHeader
        title="Settings"
        description="Organization details and the actions that cannot be undone."
      />

      <SectionCard
        title="Organization"
        description="The name your team sees in the workspace switcher."
      >
        <SettingsRow
          label="Name"
          htmlFor={canRename ? nameId : undefined}
          description={
            canRename
              ? `Up to ${String(ORG_NAME_MAX_LENGTH)} characters.`
              : 'Only organization administrators and owners can rename it.'
          }
        >
          {canRename ? (
            <div className="flex flex-col gap-1">
              <Input
                id={nameId}
                value={value}
                className="w-64"
                autoComplete="off"
                disabled={renameOrg.isPending}
                aria-invalid={nameError !== undefined}
                aria-describedby={
                  nameError === undefined ? undefined : nameErrorId
                }
                onChange={(event) => {
                  setDraft(event.target.value);
                  setNameError(undefined);
                  renameOrg.reset();
                }}
                onKeyDown={(event) => {
                  if (event.key !== 'Enter') return;
                  event.preventDefault();
                  save();
                }}
              />
              {nameError === undefined ? null : (
                <p
                  id={nameErrorId}
                  role="alert"
                  className="text-destructive text-xs"
                >
                  {nameError}
                </p>
              )}
            </div>
          ) : (
            <span className="text-sm">{title}</span>
          )}
        </SettingsRow>
      </SectionCard>

      {canDelete ? (
        <SectionCard
          title="Danger zone"
          className="border-destructive/40"
          description="Deleting the organization removes its members immediately."
        >
          <SettingsRow
            label="Delete organization"
            description="Apps must be moved or deleted first."
          >
            <Button
              type="button"
              variant="destructive"
              disabled={hasApps}
              title={hasApps ? APPS_BLOCK_DELETE : undefined}
              aria-describedby={hasApps ? appsBlockId : undefined}
              onClick={() => {
                deleteOrg.reset();
                setDeleteOpen(true);
              }}
            >
              Delete org
            </Button>
          </SettingsRow>
          {hasApps ? (
            <p id={appsBlockId} className="text-warning mt-3 text-xs">
              {APPS_BLOCK_DELETE}
            </p>
          ) : null}
          {deleteOrg.isError ? (
            <p role="alert" className="text-destructive mt-3 text-sm">
              {orgErrorMessage(deleteOrg.error, DELETE_ERROR)}
            </p>
          ) : null}
        </SectionCard>
      ) : null}

      <UnsavedChangesBar
        dirty={canRename && dirty}
        saving={renameOrg.isPending}
        error={
          renameOrg.isError
            ? orgErrorMessage(renameOrg.error, RENAME_ERROR)
            : null
        }
        onReset={() => {
          setDraft(undefined);
          setNameError(undefined);
          renameOrg.reset();
        }}
        onSave={save}
      />

      {deleteOpen ? (
        <TypedConfirmDialog
          open
          onOpenChange={(open) => {
            if (!open) setDeleteOpen(false);
          }}
          title={`Delete ${title}?`}
          description="Every member loses access to the organization and its apps."
          consequences={[
            'The organization and its member list are deleted.',
            'This cannot be undone.',
          ]}
          confirmText={title}
          confirmLabel="Delete org"
          confirming={deleteOrg.isPending}
          onConfirm={() => {
            deleteOrg.mutate(undefined, {
              onSuccess: () => {
                setDeleteOpen(false);
                toastManager.add({ type: 'success', title: 'Org deleted' });
                void router.navigate({ href: '/' } as NavigateOptions);
              },
            });
          }}
        />
      ) : null}
    </div>
  );
}
