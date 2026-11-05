import { useEffect, useMemo, useState } from 'react';
import { ErrorState } from '@/components/feedback/error-state';
import { PageHeader } from '@/components/layout/page-header';
import { UnsavedChangesBar } from '@/components/layout/unsaved-changes-bar';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { isApiError } from '@/lib/api/client';
import type { App, EffectiveRole } from '@/lib/api/endpoints/schemas';
import {
  clearPermissionDraft,
  readPermissionDraft,
  writePermissionDraft,
} from '@/lib/permissions/draft';
import {
  canEditPermissions,
  canTestPermissions,
  parseRules,
  ruleError,
  ruleNamespaces,
  rulesJson,
  type Rules,
} from '@/lib/permissions/model';
import {
  useDraftValidation,
  usePermissionsSchema,
  useRulesApp,
  useSaveRules,
} from '@/lib/permissions/queries';
import { editorNamespaces } from '@/lib/permissions/document';
import type { PermissionAction } from '@/lib/permissions/effective';
import { permissionsJsonSchema } from '@/lib/permissions/schema';
import {
  permissionsTestSearch,
  type PermissionsSearch,
  type PermissionView,
} from '@/lib/permissions/url-state';
import { toastManager } from '@/lib/toast';
import { PermissionsDiff } from './diff-view';
import { PermissionsEditor } from './permissions-editor';
import { PermissionsHistoryView } from './history-view';
import { PermissionsJsonView } from './json-view';
import { PermissionsOverview } from './permissions-overview';
import { PermissionsTestAsSheet } from './test-as-sheet';

export interface PermissionsPageProps {
  app: App;
  role: EffectiveRole;
  search: PermissionsSearch;
  onSearchChange: (search: PermissionsSearch) => void;
}

/** C.10 Permissions: the role gate and page header live outside the data gate. */
export function PermissionsPage({
  app,
  role,
  search,
  onSearchChange,
}: PermissionsPageProps) {
  return (
    <div className="flex flex-col gap-4">
      <PageHeader
        title="Permissions"
        description="CEL rules decide who can read and write each namespace."
      />
      {canEditPermissions(role) ? (
        <PermissionsWorkspace
          app={app}
          search={search}
          canTest={canTestPermissions(role)}
          onSearchChange={onSearchChange}
        />
      ) : (
        <ErrorState
          thing="permissions"
          status={403}
          requiredRole="collaborator"
          message="The permissions screen needs collaborator access to this app."
        />
      )}
    </div>
  );
}

interface PermissionsWorkspaceProps {
  app: App;
  search: PermissionsSearch;
  /** Test-as needs role A; collaborators get the editor without it. */
  canTest: boolean;
  onSearchChange: PermissionsPageProps['onSearchChange'];
}

interface RulesDraft {
  base: string;
  text: string;
}

function PermissionsWorkspace({
  app,
  search,
  canTest,
  onSearchChange,
}: PermissionsWorkspaceProps) {
  const details = useRulesApp(app.id);
  const currentApp = details.data?.app ?? app;
  const savedText = useMemo(
    () => rulesJson(currentApp.rules),
    [currentApp.rules],
  );

  const [draft, setDraft] = useState<RulesDraft | null>(() => {
    const recovered = readPermissionDraft(app.id, rulesJson(app.rules));
    return recovered.recovered
      ? { base: recovered.base, text: recovered.draft }
      : null;
  });
  const [saveOpen, setSaveOpen] = useState(false);
  const [dialogError, setDialogError] = useState<string>();
  const [focusAction, setFocusAction] = useState<PermissionAction | null>(null);
  const save = useSaveRules(app.id);

  // A fresh app payload moves an untouched draft forward; a dirty draft stays.
  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect -- a fresh app payload moves an untouched draft forward
    setDraft((previous) => {
      if (previous === null || previous.text !== previous.base) return previous;
      if (previous.base === savedText) return previous;
      return { base: savedText, text: savedText };
    });
  }, [savedText]);

  const draftText = draft?.text ?? savedText;
  const dirty = draftText !== savedText;
  const activeView: PermissionView = search.view;
  const namespaces = useMemo(
    () => ruleNamespaces(currentApp.rules),
    [currentApp.rules],
  );
  const schema = useMemo(() => permissionsJsonSchema(namespaces), [namespaces]);
  const draftRules = useMemo(() => {
    try {
      return parseRules(draftText);
    } catch {
      return null;
    }
  }, [draftText]);
  const savedRules = useMemo((): Rules => {
    const rules: unknown = currentApp.rules;
    return rules !== null && typeof rules === 'object' && !Array.isArray(rules)
      ? (rules as Rules)
      : {};
  }, [currentApp.rules]);
  const permissionsSchema = usePermissionsSchema(app.id);
  const overviewRules = draftRules ?? savedRules;
  const editorNamespaceList = useMemo(
    () =>
      editorNamespaces(
        overviewRules,
        permissionsSchema.data?.namespaces.map((entry) => entry.name) ?? [],
      ),
    [overviewRules, permissionsSchema.data],
  );
  const validation = useDraftValidation(app.id, draftRules, dirty);

  function changeDraft(value: string): void {
    setDraft({ base: savedText, text: value });
    writePermissionDraft(app.id, savedText, value);
    save.reset();
  }

  function resetDraft(): void {
    setDraft(null);
    clearPermissionDraft(app.id);
    setSaveOpen(false);
    setDialogError(undefined);
    save.reset();
  }

  function reviewSave(): void {
    if (!dirty) return;
    setDialogError(undefined);
    save.reset();
    setSaveOpen(true);
  }

  function confirmSave(): void {
    let code: Rules;
    try {
      code = parseRules(draftText);
    } catch (failure) {
      setDialogError(
        failure instanceof Error
          ? failure.message
          : 'Rules must be valid JSON.',
      );
      return;
    }
    setDialogError(undefined);
    save.mutate(code, {
      onSuccess: () => {
        setSaveOpen(false);
        setDraft(null);
        clearPermissionDraft(app.id);
        toastManager.add({ type: 'success', title: 'Permissions saved' });
      },
    });
  }

  function restoreVersion(rules: Rules): void {
    const text = rulesJson(rules);
    setDraft({ base: savedText, text });
    writePermissionDraft(app.id, savedText, text);
    setSaveOpen(false);
    setDialogError(undefined);
    save.reset();
    changeView('json');
    toastManager.add({
      type: 'success',
      title: 'Version loaded into the draft',
      description: 'Save to apply it.',
    });
  }

  function changeView(view: PermissionView): void {
    setFocusAction(null);
    onSearchChange({ ...search, view });
  }

  function changeNamespace(namespace: string): void {
    setFocusAction(null);
    onSearchChange({ ...search, view: 'editor', ns: namespace });
  }

  function openOverviewCell(
    namespace: string,
    action?: PermissionAction,
  ): void {
    setFocusAction(action ?? null);
    onSearchChange({ ...search, view: 'editor', ns: namespace });
  }

  function openTest(): void {
    onSearchChange(permissionsTestSearch(search, true));
  }

  function closeTest(): void {
    onSearchChange(permissionsTestSearch(search, false));
  }

  if (details.isError && details.data === undefined) {
    return (
      <ErrorState
        thing="permission rules"
        status={isApiError(details.error) ? details.error.status : undefined}
        message={
          details.error instanceof Error ? details.error.message : undefined
        }
        type={isApiError(details.error) ? details.error.type : undefined}
        hint={isApiError(details.error) ? details.error.hint : undefined}
        requestId={
          isApiError(details.error) ? details.error.requestId : undefined
        }
        onRetry={() => {
          void details.refetch();
        }}
      />
    );
  }

  const saveError = save.isError ? ruleError(save.error) : undefined;
  const dialogErrorText = dialogError ?? saveError;

  return (
    <div className="flex flex-col gap-4">
      <Tabs
        value={activeView}
        className="flex flex-col gap-4"
        onValueChange={(value) => {
          if (
            value === 'editor' ||
            value === 'json' ||
            value === 'overview' ||
            value === 'history'
          ) {
            changeView(value as PermissionView);
          }
        }}
      >
        <TabsList aria-label="Permission views">
          <TabsTrigger value="editor">Editor</TabsTrigger>
          <TabsTrigger value="json">JSON</TabsTrigger>
          <TabsTrigger value="overview">Overview</TabsTrigger>
          <TabsTrigger value="history">History</TabsTrigger>
        </TabsList>

        <TabsContent value="editor">
          {draftRules === null ? (
            <div
              role="alert"
              className="border-destructive/40 bg-destructive/5 rounded-lg border p-4"
            >
              <p className="font-medium">
                The draft isn&apos;t a valid rules document, so the visual
                editor can&apos;t open it.
              </p>
              <Button
                type="button"
                variant="secondary"
                size="sm"
                className="mt-2"
                onClick={() => {
                  changeView('json');
                }}
              >
                Open JSON
              </Button>
            </div>
          ) : (
            <PermissionsEditor
              rules={draftRules}
              schema={permissionsSchema.data}
              validation={validation}
              dirty={dirty}
              namespace={search.ns}
              focusAction={focusAction}
              onChange={(next) => {
                changeDraft(rulesJson(next));
              }}
              onNamespaceChange={changeNamespace}
              onOpenTest={canTest ? openTest : undefined}
              onOpenJson={() => {
                changeView('json');
              }}
            />
          )}
        </TabsContent>

        <TabsContent value="json">
          <PermissionsJsonView
            value={draftText}
            schema={schema}
            onChange={changeDraft}
          />
        </TabsContent>

        <TabsContent value="overview">
          <PermissionsOverview
            rules={overviewRules}
            schema={permissionsSchema.data}
            onOpen={openOverviewCell}
          />
        </TabsContent>

        <TabsContent value="history">
          <PermissionsHistoryView
            appId={app.id}
            rules={currentApp.rules}
            rulesVersion={currentApp.rules_version}
            enabled={activeView === 'history'}
            onRestore={restoreVersion}
          />
        </TabsContent>
      </Tabs>

      <UnsavedChangesBar
        dirty={dirty}
        saving={save.isPending}
        error={saveError ?? null}
        onReset={resetDraft}
        onSave={reviewSave}
      />

      <Dialog
        open={saveOpen}
        onOpenChange={(open) => {
          if (save.isPending) return;
          setSaveOpen(open);
          if (!open) {
            setDialogError(undefined);
            save.reset();
          }
        }}
      >
        <DialogContent className="max-w-[880px]">
          <DialogHeader>
            <DialogTitle>Review permission changes</DialogTitle>
            <DialogDescription>
              Saving replaces the app&apos;s permission rules immediately.
              Requests already in flight finish with the old rules.
            </DialogDescription>
          </DialogHeader>

          <div className="mt-3 max-h-[60vh] overflow-auto">
            <PermissionsDiff
              before={savedText}
              after={draftText}
              label="Current → draft"
            />
          </div>

          {dialogErrorText !== undefined ? (
            <p
              role="alert"
              className="text-destructive mt-3 text-sm whitespace-pre-line"
            >
              {dialogErrorText}
            </p>
          ) : null}

          <DialogFooter>
            <Button
              type="button"
              variant="secondary"
              disabled={save.isPending}
              onClick={() => {
                setSaveOpen(false);
                setDialogError(undefined);
                save.reset();
              }}
            >
              Cancel
            </Button>
            <Button
              type="button"
              disabled={save.isPending}
              onClick={confirmSave}
            >
              {save.isPending ? 'Saving…' : 'Save rules'}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {canTest ? (
        <PermissionsTestAsSheet
          open={search.test !== undefined}
          onOpenChange={(open) => {
            if (!open) closeTest();
          }}
          app={currentApp}
          draftRules={draftRules}
          namespaces={editorNamespaceList}
        />
      ) : null}
    </div>
  );
}
