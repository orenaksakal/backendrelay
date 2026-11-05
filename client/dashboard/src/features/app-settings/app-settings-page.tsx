import { useRouter, type NavigateOptions } from '@tanstack/react-router';
import { useId, useState } from 'react';
import { CopyButton } from '@/components/data/copy-button';
import { IdChip } from '@/components/data/id-chip';
import { RelativeTime } from '@/components/data/relative-time';
import { SecretField } from '@/components/data/secret-field';
import { ErrorState } from '@/components/feedback/error-state';
import { StatusPill } from '@/components/feedback/status-pill';
import { CardSkeleton, TableSkeleton } from '@/components/feedback/skeletons';
import { TypedConfirmDialog } from '@/components/feedback/typed-confirm-dialog';
import { PageHeader } from '@/components/layout/page-header';
import { SectionCard } from '@/components/layout/section-card';
import { SettingsRow } from '@/components/layout/settings-row';
import { UnsavedChangesBar } from '@/components/layout/unsaved-changes-bar';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { Switch } from '@/components/ui/switch';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { isApiError } from '@/lib/api/client';
import type { App, EffectiveRole } from '@/lib/api/endpoints/schemas';
import { effectiveAppStatus } from '@/lib/apps/apps-home';
import {
  APP_NAME_MAX_LENGTH,
  appNameProblem,
  creationErrorMessage,
} from '@/lib/creation/creation';
import {
  APP_SETTINGS_TABS,
  APP_SETTINGS_TAB_LABELS,
  canClearAppData,
  canDeleteApp,
  canPauseWrites,
  canRenameApp,
  canTransferApp,
  canViewAdminToken,
  isAppSettingsTab,
  isWritesPaused,
  statusForPause,
  transferableOrgs,
  type AppSettingsTab,
} from '@/lib/apps/settings';
import {
  useAppDetails,
  useClearApp,
  useDeleteApp,
  useRenameApp,
  useRotateAppAdminToken,
  useSetAppStatus,
  useTransferApp,
} from '@/lib/apps/settings-queries';
import { toastManager } from '@/lib/toast';
import { useBootstrap } from '@/lib/workspace/queries';
import { PERSONAL_WORKSPACE_ID } from '@/lib/workspace/workspaces';
import { AppTeamSection } from './app-team-section';

const RENAME_ERROR = "Couldn't rename the app. Try again.";
const STATUS_ERROR = "Couldn't change the app status. Try again.";
const ROTATE_ERROR = "Couldn't rotate the admin token. Try again.";
const TRANSFER_ERROR = "Couldn't transfer the app. Try again.";
const CLEAR_ERROR = "Couldn't clear the app data. Try again.";
const DELETE_ERROR = "Couldn't delete the app. Try again.";

const ROTATE_WARNING =
  'Rotating breaks every server that uses the old token until you update it.';

export interface AppSettingsPageProps {
  app: App;
  role: EffectiveRole;
  workspaceId: string;
  tab: AppSettingsTab;
  onTabChange: (tab: AppSettingsTab) => void;
}

/**
 * C.16 app settings: general details and status, the admin token, the team and
 * the danger zone. The app record is refetched because the layout's copy comes
 * from the bootstrap list, which has no members or invites.
 */
export function AppSettingsPage({
  app: routeApp,
  role,
  workspaceId,
  tab,
  onTabChange,
}: AppSettingsPageProps) {
  const details = useAppDetails(routeApp.id);
  const app = details.data?.app ?? routeApp;
  const isPersonal = workspaceId === PERSONAL_WORKSPACE_ID;

  const rename = useRenameApp(app.id);
  const [titleDraft, setTitleDraft] = useState<string>();
  const [titleError, setTitleError] = useState<string>();

  const titleValue = titleDraft ?? app.title;
  const titleDirty =
    titleDraft !== undefined && titleDraft.trim() !== app.title;

  function resetTitle(): void {
    setTitleDraft(undefined);
    setTitleError(undefined);
    rename.reset();
  }

  function saveTitle(): void {
    if (!titleDirty || rename.isPending) return;
    const problem = appNameProblem(titleValue);
    if (problem !== undefined) {
      setTitleError(problem);
      return;
    }
    setTitleError(undefined);
    rename.mutate(titleValue.trim(), {
      onSuccess: () => {
        setTitleDraft(undefined);
        toastManager.add({ type: 'success', title: 'App renamed' });
      },
    });
  }

  return (
    <div className="flex max-w-4xl flex-col gap-4">
      <Tabs
        value={tab}
        className="flex flex-col gap-4"
        onValueChange={(value) => {
          if (isAppSettingsTab(value)) onTabChange(value);
        }}
      >
        <PageHeader
          title="Settings"
          description="App details, status, the admin token, the team and deletion."
          tabs={
            <TabsList aria-label="App settings">
              {APP_SETTINGS_TABS.map((value) => (
                <TabsTrigger key={value} value={value}>
                  {APP_SETTINGS_TAB_LABELS[value]}
                </TabsTrigger>
              ))}
            </TabsList>
          }
        />

        <TabsContent value="general">
          <GeneralTab
            app={app}
            role={role}
            isPersonal={isPersonal}
            workspaceId={workspaceId}
            titleValue={titleValue}
            titleError={titleError}
            renaming={rename.isPending}
            onTitleChange={(value) => {
              setTitleDraft(value);
              setTitleError(undefined);
              rename.reset();
            }}
          />
        </TabsContent>

        <TabsContent value="token">
          <AdminTokenTab app={app} role={role} loading={details.isPending} />
        </TabsContent>

        <TabsContent value="team">
          {details.isError && details.data === undefined ? (
            <ErrorState
              thing="the app team"
              status={
                isApiError(details.error) ? details.error.status : undefined
              }
              message={
                details.error instanceof Error
                  ? details.error.message
                  : undefined
              }
              onRetry={() => {
                void details.refetch();
              }}
            />
          ) : details.isPending ? (
            <TableSkeleton rows={3} columns={3} />
          ) : (
            <AppTeamSection app={app} role={role} workspaceId={workspaceId} />
          )}
        </TabsContent>

        <TabsContent value="danger">
          <DangerZoneTab
            app={app}
            role={role}
            workspaceId={workspaceId}
            isPersonal={isPersonal}
          />
        </TabsContent>
      </Tabs>

      <UnsavedChangesBar
        dirty={canRenameApp(role) && titleDirty}
        saving={rename.isPending}
        error={
          rename.isError
            ? creationErrorMessage(rename.error, RENAME_ERROR)
            : null
        }
        onReset={resetTitle}
        onSave={saveTitle}
      />
    </div>
  );
}

interface GeneralTabProps {
  app: App;
  role: EffectiveRole;
  isPersonal: boolean;
  workspaceId: string;
  titleValue: string;
  titleError: string | undefined;
  renaming: boolean;
  onTitleChange: (value: string) => void;
}

function GeneralTab({
  app,
  role,
  isPersonal,
  workspaceId,
  titleValue,
  titleError,
  renaming,
  onTitleChange,
}: GeneralTabProps) {
  const canRename = canRenameApp(role);
  const nameId = useId();
  const nameErrorId = `${nameId}-error`;
  const pauseLabelId = useId();

  const setStatus = useSetAppStatus(app.id);
  const status = effectiveAppStatus(app);
  const disabledByOperator = status === 'disabled';
  const canPause = canPauseWrites(role);
  const pauseReason = disabledByOperator
    ? 'This app was disabled by the operator.'
    : canPause
      ? undefined
      : 'Only app administrators and owners can pause writes.';

  return (
    <div className="flex flex-col gap-4">
      <SectionCard
        title="App"
        description="The name and identifiers used across the dashboard."
      >
        <div className="flex flex-col gap-4">
          <SettingsRow
            label="Name"
            htmlFor={canRename ? nameId : undefined}
            description={
              canRename
                ? `Up to ${String(APP_NAME_MAX_LENGTH)} characters.`
                : 'Only app owners can rename it.'
            }
          >
            {canRename ? (
              <div className="flex flex-col gap-1">
                <Input
                  id={nameId}
                  value={titleValue}
                  className="w-64"
                  autoComplete="off"
                  disabled={renaming}
                  aria-invalid={titleError !== undefined}
                  aria-describedby={
                    titleError === undefined ? undefined : nameErrorId
                  }
                  onChange={(event) => {
                    onTitleChange(event.target.value);
                  }}
                />
                {titleError === undefined ? null : (
                  <p
                    id={nameErrorId}
                    role="alert"
                    className="text-destructive text-xs"
                  >
                    {titleError}
                  </p>
                )}
              </div>
            ) : (
              <span className="text-sm">{app.title}</span>
            )}
          </SettingsRow>

          <SettingsRow label="App ID">
            <IdChip value={app.id} />
          </SettingsRow>

          <SettingsRow label="Created">
            <RelativeTime value={app.created_at} className="text-cell" />
          </SettingsRow>

          <SettingsRow label="Workspace">
            <WorkspaceLink
              app={app}
              isPersonal={isPersonal}
              workspaceId={workspaceId}
            />
          </SettingsRow>
        </div>
      </SectionCard>

      <SectionCard
        title="Status"
        description="Pause writes before maintenance or a migration."
        actions={<StatusPill status={status} />}
      >
        <div className="flex flex-col gap-3">
          <SettingsRow
            label="Pause writes"
            labelId={pauseLabelId}
            description={
              pauseReason ??
              'New writes are rejected while paused; reads keep working.'
            }
          >
            <Switch
              aria-labelledby={pauseLabelId}
              checked={isWritesPaused(status)}
              disabled={!canPause || disabledByOperator || setStatus.isPending}
              title={pauseReason}
              onCheckedChange={(checked) => {
                setStatus.reset();
                setStatus.mutate(statusForPause(checked));
              }}
            />
          </SettingsRow>

          {disabledByOperator ? (
            <p className="text-warning text-xs" role="status">
              This app was disabled by the operator.
            </p>
          ) : null}

          {setStatus.isError ? (
            <p role="alert" className="text-destructive text-sm">
              {creationErrorMessage(setStatus.error, STATUS_ERROR)}
            </p>
          ) : null}
        </div>
      </SectionCard>
    </div>
  );
}

function WorkspaceLink({
  app,
  isPersonal,
  workspaceId,
}: {
  app: App;
  isPersonal: boolean;
  workspaceId: string;
}) {
  if (isPersonal) return <span className="text-sm">Personal</span>;
  const title = app.org?.title ?? workspaceId;
  return (
    <a
      href={`/w/${encodeURIComponent(workspaceId)}/settings`}
      className="text-accent text-sm hover:underline"
    >
      {title}
    </a>
  );
}

function AdminTokenTab({
  app,
  role,
  loading,
}: {
  app: App;
  role: EffectiveRole;
  loading: boolean;
}) {
  const rotate = useRotateAppAdminToken(app.id);
  const [rotated, setRotated] = useState<string>();

  if (!canViewAdminToken(role)) {
    return (
      <ErrorState
        thing="the admin token"
        status={403}
        requiredRole="admin"
        message="Only app administrators and owners can see or rotate the admin token."
      />
    );
  }

  if (loading) return <CardSkeleton />;

  return (
    <div className="flex flex-col gap-4">
      <SectionCard
        title="Admin token"
        description="The credential your servers use to write to this app."
      >
        <div className="flex flex-col gap-3">
          {app.admin_token === undefined ? (
            <p className="text-muted-foreground text-sm">
              The admin token is not available. Reload the page to try again.
            </p>
          ) : (
            <SecretField
              value={app.admin_token}
              onRotate={() => {
                rotate.reset();
                rotate.mutate(undefined, {
                  onSuccess: (result) => {
                    setRotated(result.adminToken);
                  },
                });
              }}
              rotateConsequences={[
                'The old token stops working immediately.',
                ROTATE_WARNING,
                'You see the new token once, on this screen.',
              ]}
            />
          )}

          <p className="text-warning text-xs">{ROTATE_WARNING}</p>

          {rotate.isError ? (
            <p role="alert" className="text-destructive text-sm">
              {creationErrorMessage(rotate.error, ROTATE_ERROR)}
            </p>
          ) : null}
        </div>
      </SectionCard>

      <Dialog
        open={rotated !== undefined}
        onOpenChange={() => {
          // The token is shown once: only "I've saved it" closes the dialog.
        }}
      >
        <DialogContent showCloseButton={false}>
          <DialogHeader>
            <DialogTitle>Copy the new admin token</DialogTitle>
            <DialogDescription>
              This is the only time the new token is shown. Update every server
              that uses the old one before you close this dialog.
            </DialogDescription>
          </DialogHeader>
          <div className="border-input bg-muted mt-4 flex items-start gap-2 rounded-md border p-3">
            <code className="text-cell min-w-0 flex-1 font-mono break-all">
              {rotated}
            </code>
            <CopyButton value={rotated ?? ''} aria-label="Copy admin token" />
          </div>
          <DialogFooter>
            <Button
              type="button"
              onClick={() => {
                setRotated(undefined);
                rotate.reset();
              }}
            >
              I&apos;ve saved it
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}

interface DangerZoneTabProps {
  app: App;
  role: EffectiveRole;
  workspaceId: string;
  isPersonal: boolean;
}

function DangerZoneTab({
  app,
  role,
  workspaceId,
  isPersonal,
}: DangerZoneTabProps) {
  const router = useRouter();
  const bootstrap = useBootstrap();
  const transfer = useTransferApp(app.id);
  const clear = useClearApp(app.id);
  const remove = useDeleteApp(app.id);

  const [transferOpen, setTransferOpen] = useState(false);
  const [clearOpen, setClearOpen] = useState(false);
  const [deleteOpen, setDeleteOpen] = useState(false);
  const [targetOrgId, setTargetOrgId] = useState<string>();

  const eligibleOrgs = transferableOrgs(
    bootstrap.data?.orgs,
    app.org?.id ?? app.org_id,
  );
  const selectedOrg =
    eligibleOrgs.find((org) => org.id === targetOrgId) ?? eligibleOrgs[0];

  const canTransfer = canTransferApp(role);
  const canClear = canClearAppData(role);
  const canDelete = canDeleteApp(role, workspaceId);

  const transferBlocked = !canTransfer
    ? 'Only app owners can transfer it.'
    : eligibleOrgs.length === 0
      ? 'You need admin access to another organization first.'
      : undefined;
  const clearBlocked = canClear
    ? undefined
    : 'Only the app owner can clear its data.';
  const deleteBlocked = canDelete
    ? undefined
    : isPersonal
      ? 'Only the app owner can delete a personal app.'
      : 'Only app administrators and owners can delete this app.';

  return (
    <div className="flex flex-col gap-4">
      <SectionCard
        title="Danger zone"
        description="These actions change or remove the app. They cannot be undone."
        className="border-destructive/40"
      >
        <div className="flex flex-col gap-4">
          <SettingsRow
            label="Transfer to org"
            description={
              transferBlocked ??
              'Move this app to an organization you administer. Its members gain access.'
            }
          >
            {eligibleOrgs.length > 0 ? (
              <Select
                value={selectedOrg?.id ?? ''}
                disabled={!canTransfer || transfer.isPending}
                onValueChange={(value) => {
                  if (typeof value === 'string') setTargetOrgId(value);
                }}
              >
                <SelectTrigger aria-label="Target org" className="w-48">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {eligibleOrgs.map((org) => (
                    <SelectItem key={org.id} value={org.id}>
                      {org.title}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            ) : null}
            <Button
              type="button"
              variant="destructive"
              disabled={
                !canTransfer || selectedOrg === undefined || transfer.isPending
              }
              title={transferBlocked}
              onClick={() => {
                transfer.reset();
                setTransferOpen(true);
              }}
            >
              Transfer app
            </Button>
          </SettingsRow>

          <SettingsRow
            label="Clear all data"
            description={
              clearBlocked ??
              'Removes every entity, attribute and permission rule. The app and its admin token stay.'
            }
          >
            <Button
              type="button"
              variant="destructive"
              disabled={!canClear || clear.isPending}
              title={clearBlocked}
              onClick={() => {
                clear.reset();
                setClearOpen(true);
              }}
            >
              Clear all data
            </Button>
          </SettingsRow>

          <SettingsRow
            label="Delete app"
            description={
              deleteBlocked ??
              'The app is scheduled for deletion and disappears from every workspace.'
            }
          >
            <Button
              type="button"
              variant="destructive"
              disabled={!canDelete || remove.isPending}
              title={deleteBlocked}
              onClick={() => {
                remove.reset();
                setDeleteOpen(true);
              }}
            >
              Delete app
            </Button>
          </SettingsRow>
        </div>
      </SectionCard>

      {transfer.isError ? (
        <p role="alert" className="text-destructive text-sm">
          {creationErrorMessage(transfer.error, TRANSFER_ERROR)}
        </p>
      ) : null}
      {clear.isError ? (
        <p role="alert" className="text-destructive text-sm">
          {creationErrorMessage(clear.error, CLEAR_ERROR)}
        </p>
      ) : null}
      {remove.isError ? (
        <p role="alert" className="text-destructive text-sm">
          {creationErrorMessage(remove.error, DELETE_ERROR)}
        </p>
      ) : null}

      <TypedConfirmDialog
        open={transferOpen}
        onOpenChange={setTransferOpen}
        title={`Transfer ${app.title}?`}
        description={`Move this app to ${selectedOrg?.title ?? 'the organization'}.`}
        consequences={[
          'The app and its data move to the target organization.',
          'The current workspace loses access to the app.',
          'Webhooks and OAuth clients are unchanged.',
        ]}
        confirmText={app.title}
        confirmLabel="Transfer app"
        confirming={transfer.isPending}
        onConfirm={() => {
          if (selectedOrg === undefined) return;
          transfer.mutate(selectedOrg.id, {
            onSuccess: () => {
              setTransferOpen(false);
              toastManager.add({ type: 'success', title: 'App transferred' });
              void router.navigate({
                href: `/w/${encodeURIComponent(selectedOrg.id)}`,
              } as NavigateOptions);
            },
          });
        }}
      />

      <TypedConfirmDialog
        open={clearOpen}
        onOpenChange={setClearOpen}
        title={`Clear all data in ${app.title}?`}
        description="Every entity is deleted and the permission rules are reset."
        consequences={[
          'Every entity, attribute and permission rule is removed.',
          'The app, its ID and its admin token stay.',
          'This cannot be undone.',
        ]}
        confirmText={app.title}
        confirmLabel="Clear all data"
        confirming={clear.isPending}
        onConfirm={() => {
          clear.mutate(undefined, {
            onSuccess: () => {
              setClearOpen(false);
              toastManager.add({ type: 'success', title: 'App data cleared' });
            },
          });
        }}
      />

      <TypedConfirmDialog
        open={deleteOpen}
        onOpenChange={setDeleteOpen}
        title={`Delete ${app.title}?`}
        description="The app is marked for deletion and its data is removed by the cleanup worker."
        consequences={[
          'Every member loses access to the app immediately.',
          'The app and its data are removed permanently.',
          'This cannot be undone.',
        ]}
        confirmText={app.title}
        confirmLabel="Delete app"
        confirming={remove.isPending}
        onConfirm={() => {
          remove.mutate(undefined, {
            onSuccess: () => {
              setDeleteOpen(false);
              toastManager.add({
                type: 'success',
                title: 'App scheduled for deletion',
              });
              void router.navigate({
                href: `/w/${encodeURIComponent(workspaceId)}`,
              } as NavigateOptions);
            },
          });
        }}
      />
    </div>
  );
}
