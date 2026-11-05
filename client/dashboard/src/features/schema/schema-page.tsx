import { useCallback, useEffect, useMemo, useState } from 'react';
import { AppDataGate } from '@/components/data/app-data-gate';
import { TypedConfirmDialog } from '@/components/feedback/typed-confirm-dialog';
import { PageHeader } from '@/components/layout/page-header';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import {
  ResizableHandle,
  ResizablePanel,
  ResizablePanelGroup,
} from '@/components/ui/resizable';
import type { App, EffectiveRole } from '@/lib/api/endpoints/schemas';
import { effectiveAllowRule, ruleAllowsEveryone } from '@/lib/apps/overview';
import { publicWriteWarningActions } from '@/lib/permissions/effective';
import { deleteNamespaceOps } from '@/lib/schema/attributes';
import { readNamespacePins, toggleNamespacePin } from '@/lib/explorer/storage';
import {
  schemaSearchFor,
  schemaTabSearch,
  SCHEMA_TAB_LABELS,
  SCHEMA_TABS,
  type SchemaSearch,
  type SchemaTab,
} from '@/lib/schema/url-state';
import { useNamespaceCounts } from '@/lib/explorer/rows';
import { peekAppClient, useAppSchema } from '@/lib/instant';
import type { AppNamespace } from '@/lib/instant';
import { showErrorToast, toastManager } from '@/lib/toast';
import { NamespaceSidebar } from '@/features/explorer/namespace-sidebar';
import { NewNamespaceDialog } from '@/features/explorer/new-namespace-dialog';
import { AttributesTab } from './attributes-tab';
import { LinksTab } from './links-tab';
import { CodeTab } from './code-tab';
import { JobsTab } from './jobs-tab';
import { DeletedTab } from './deleted-tab';

export interface SchemaPageProps {
  app: App;
  role: EffectiveRole;
  search: SchemaSearch;
  onSearchChange: (search: SchemaSearch) => void;
}

/** C.7 Schema: the role gate and page header live outside the data gate. */
export function SchemaPage({
  app,
  role,
  search,
  onSearchChange,
}: SchemaPageProps) {
  return (
    <div className="flex flex-col gap-4">
      <PageHeader
        title="Schema"
        description="Attributes, links and the generated schema file for this app."
      />
      <AppDataGate app={app} role={role} thing="schema">
        <SchemaWorkspace
          app={app}
          search={search}
          onSearchChange={onSearchChange}
        />
      </AppDataGate>
    </div>
  );
}

interface SchemaWorkspaceProps {
  app: App;
  search: SchemaSearch;
  onSearchChange: SchemaPageProps['onSearchChange'];
}

function SchemaWorkspace({
  app,
  search,
  onSearchChange,
}: SchemaWorkspaceProps) {
  const client = peekAppClient(app.id);
  const schema = useAppSchema(app.id);
  const namespaces = schema.namespaces;

  const selected = useMemo(() => {
    const explicit = namespaces.find(
      (candidate) => candidate.name === search.ns,
    );
    if (explicit !== undefined) return explicit;
    return (
      namespaces.find((candidate) => !candidate.isSystem) ??
      namespaces[0] ??
      null
    );
  }, [namespaces, search.ns]);

  // Opening the Schema screen without `?ns=` selects the first user namespace.
  useEffect(() => {
    if (schema.isLoading || selected === null) return;
    if (search.ns === selected.name) return;
    onSearchChange(schemaSearchFor(selected.name, search.tab));
  }, [schema.isLoading, search.ns, search.tab, selected, onSearchChange]);

  const namespaceNames = useMemo(
    () => namespaces.map((candidate) => candidate.name),
    [namespaces],
  );
  const counts = useNamespaceCounts(client ?? null, namespaceNames);

  const warningNamespaces = useMemo(
    () =>
      namespaces
        .filter((candidate) =>
          publicWriteWarningActions(candidate.name).some((action) =>
            ruleAllowsEveryone(
              effectiveAllowRule(app.rules, candidate.name, action),
            ),
          ),
        )
        .map((candidate) => candidate.name),
    [app.rules, namespaces],
  );

  const [pins, setPins] = useState<string[]>(() => readNamespacePins(app.id));
  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect -- pins are stored per app in localStorage
    setPins(readNamespacePins(app.id));
  }, [app.id]);

  const [newNamespaceOpen, setNewNamespaceOpen] = useState(false);
  const [deleteTarget, setDeleteTarget] = useState<AppNamespace | null>(null);
  const [deletePending, setDeletePending] = useState(false);

  const selectNamespace = useCallback(
    (name: string) => {
      onSearchChange(schemaSearchFor(name, search.tab));
    },
    [onSearchChange, search.tab],
  );

  const changeTab = useCallback(
    (tab: SchemaTab) => {
      onSearchChange(schemaTabSearch(search, tab));
    },
    [onSearchChange, search],
  );

  async function confirmNamespaceDelete(): Promise<void> {
    if (deleteTarget === null || client === undefined) return;
    setDeletePending(true);
    try {
      await client.core._reactor.pushOps(deleteNamespaceOps(deleteTarget));
      toastManager.add({
        type: 'success',
        title: `Namespace ${deleteTarget.name} deleted`,
      });
      setDeleteTarget(null);
    } catch (failure) {
      showErrorToast(failure);
    } finally {
      setDeletePending(false);
    }
  }

  if (client === undefined) return null;

  return (
    <div className="flex flex-col gap-3">
      <p className="text-muted-foreground text-xs md:hidden">
        The schema editor works best on a larger screen.
      </p>

      <ResizablePanelGroup
        orientation="horizontal"
        className="min-h-[60vh] gap-2"
      >
        <ResizablePanel
          defaultSize="20%"
          minSize="15%"
          maxSize="33%"
          className="min-w-0 lg:max-w-[400px] lg:min-w-[200px]"
        >
          <NamespaceSidebar
            namespaces={namespaces}
            counts={counts}
            publicWrite={warningNamespaces}
            selected={selected?.name ?? null}
            pins={pins}
            isLoading={schema.isLoading}
            onSelect={selectNamespace}
            onTogglePin={(name) => {
              setPins(toggleNamespacePin(app.id, name));
            }}
            onNewNamespace={() => {
              setNewNamespaceOpen(true);
            }}
            onDeleteNamespace={(name) => {
              const target = namespaces.find(
                (candidate) => candidate.name === name,
              );
              if (target !== undefined) setDeleteTarget(target);
            }}
          />
        </ResizablePanel>
        <ResizableHandle />
        <ResizablePanel defaultSize="80%" className="min-w-0 pl-2">
          <Tabs
            value={search.tab}
            className="flex flex-col gap-4"
            onValueChange={(value) => {
              if (SCHEMA_TABS.some((tab) => tab === value)) {
                changeTab(value as SchemaTab);
              }
            }}
          >
            <TabsList aria-label="Schema views">
              {SCHEMA_TABS.map((tab) => (
                <TabsTrigger key={tab} value={tab}>
                  {SCHEMA_TAB_LABELS[tab]}
                </TabsTrigger>
              ))}
            </TabsList>

            <TabsContent value="attributes">
              <AttributesTab
                appId={app.id}
                namespace={selected}
                rawAttrs={schema.attrs}
                client={client}
              />
            </TabsContent>
            <TabsContent value="links">
              <LinksTab
                namespace={selected}
                namespaces={namespaces}
                rawAttrs={schema.attrs}
                client={client}
              />
            </TabsContent>
            <TabsContent value="code">
              <CodeTab attrs={schema.attrs} />
            </TabsContent>
            <TabsContent value="jobs">
              <JobsTab appId={app.id} />
            </TabsContent>
            <TabsContent value="deleted">
              <DeletedTab appId={app.id} client={client} />
            </TabsContent>
          </Tabs>
        </ResizablePanel>
      </ResizablePanelGroup>

      <NewNamespaceDialog
        open={newNamespaceOpen}
        onOpenChange={setNewNamespaceOpen}
        client={client}
        existing={namespaceNames}
        onCreated={selectNamespace}
      />

      <TypedConfirmDialog
        open={deleteTarget !== null}
        onOpenChange={(open) => {
          if (!open) setDeleteTarget(null);
        }}
        title={`Delete namespace ${deleteTarget?.name ?? ''}?`}
        description="Every attribute and link that defines this namespace is soft-deleted."
        consequences={[
          `All entities in ${deleteTarget?.name ?? 'the namespace'} lose their attributes and links.`,
          'The attributes can be restored from the Deleted tab until the grace period ends.',
        ]}
        confirmText={deleteTarget?.name ?? ''}
        confirmLabel="Delete namespace"
        confirming={deletePending}
        onConfirm={() => {
          void confirmNamespaceDelete();
        }}
      />
    </div>
  );
}
