import { Pause, Play, Plus, RefreshCw, Table2 } from 'lucide-react';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { FilterButton } from '@/components/data/filter-panel';
import type {
  FilterAttribute,
  InstaqlWhere,
} from '@/components/data/filter-serialization';
import { SearchInput } from '@/components/data/search-input';
import { AppDataGate } from '@/components/data/app-data-gate';
import { EmptyState } from '@/components/feedback/empty-state';
import { ErrorState } from '@/components/feedback/error-state';
import { TableSkeleton } from '@/components/feedback/skeletons';
import { PageHeader } from '@/components/layout/page-header';
import { Button } from '@/components/ui/button';
import {
  ResizableHandle,
  ResizablePanel,
  ResizablePanelGroup,
} from '@/components/ui/resizable';
import type { App, EffectiveRole } from '@/lib/api/endpoints/schemas';
import { effectiveAllowRule, ruleAllowsEveryone } from '@/lib/apps/overview';
import { publicWriteWarningActions } from '@/lib/permissions/effective';
import { explorerColumns, visibleColumns } from '@/lib/explorer/columns';
import { entityCountLabel } from '@/lib/explorer/entities';
import { parseSearchInput } from '@/lib/explorer/search';
import {
  readHiddenColumns,
  readNamespacePins,
  toggleHiddenColumn,
  toggleNamespacePin,
} from '@/lib/explorer/storage';
import {
  EXPLORER_PAGE_STEP,
  MAX_EXPLORER_PAGE_SIZE,
  explorerSearchFor,
  readExplorerUrlState,
  resolveSort,
  serializeExplorerWhere,
  type ExplorerSearch,
  type ExplorerSort,
  type ExplorerUrlState,
} from '@/lib/explorer/url-state';
import { useExplorerRows, useNamespaceCounts } from '@/lib/explorer/rows';
import type { ExplorerRow } from '@/lib/explorer/rows';
import {
  deleteEntities,
  downloadJson,
  entityExport,
  parseCellValue,
  updateCell,
} from '@/lib/explorer/writes';
import { peekAppClient, useAppSchema } from '@/lib/instant';
import type { AppAttribute } from '@/lib/instant';
import { formatCount } from '@/lib/format';
import { PAGE_ACTION_EVENT } from '@/lib/shortcuts';
import { showErrorToast, toastManager } from '@/lib/toast';
import { NamespaceSidebar } from './namespace-sidebar';
import { ColumnsMenu } from './columns-menu';
import { ExplorerGrid } from './explorer-grid';
import { BulkActions } from './bulk-actions';
import { EntitySheet } from './entity-sheet';
import { NewNamespaceDialog } from './new-namespace-dialog';

export interface ExplorerPageProps {
  app: App;
  role: EffectiveRole;
  search: ExplorerSearch;
  /** Route-owned URL write; `replace` keeps auto-selection out of history. */
  onSearchChange: (
    search: ExplorerSearch,
    options?: { replace?: boolean },
  ) => void;
}

const NO_WHERE: InstaqlWhere = {};

/** C.6 Explorer: the role gate and page header live outside the data gate. */
export function ExplorerPage({
  app,
  role,
  search,
  onSearchChange,
}: ExplorerPageProps) {
  return (
    <div className="flex flex-col gap-4">
      <PageHeader
        title="Explorer"
        description="Read every namespace in this app, live."
      />
      <AppDataGate app={app} role={role} thing="explorer">
        <ExplorerWorkspace
          app={app}
          search={search}
          onSearchChange={onSearchChange}
        />
      </AppDataGate>
    </div>
  );
}

interface ExplorerWorkspaceProps {
  app: App;
  search: ExplorerSearch;
  onSearchChange: ExplorerPageProps['onSearchChange'];
}

function ExplorerWorkspace({
  app,
  search,
  onSearchChange,
}: ExplorerWorkspaceProps) {
  const client = peekAppClient(app.id);
  const schema = useAppSchema(app.id);
  const namespaces = schema.namespaces;
  const toolbarRef = useRef<HTMLDivElement>(null);

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

  // Opening the Explorer without `?ns=` selects the first user namespace.
  useEffect(() => {
    if (schema.isLoading || selected === null) return;
    if (search.ns === selected.name) return;
    onSearchChange(
      explorerSearchFor(
        {
          namespace: selected.name,
          text: '',
          conditions: [],
          sort: null,
          size: search.size,
        },
        [],
      ),
      { replace: true },
    );
  }, [schema.isLoading, search.ns, search.size, selected, onSearchChange]);

  const attributes = useMemo<FilterAttribute[]>(
    () =>
      (selected?.attributes ?? []).map((attribute) => ({
        name: attribute.name,
        type: attribute.type,
        indexed: attribute.isIndexed,
      })),
    [selected],
  );

  const view = useMemo(
    () => readExplorerUrlState(search, attributes),
    [search, attributes],
  );
  const where = useMemo(
    () => serializeExplorerWhere(view, attributes) ?? NO_WHERE,
    [view, attributes],
  );
  const sort = useMemo(
    () => resolveSort(view.sort, attributes),
    [view, attributes],
  );

  const namespaceNames = useMemo(
    () => namespaces.map((candidate) => candidate.name),
    [namespaces],
  );
  const counts = useNamespaceCounts(client ?? null, namespaceNames);
  const linkLabels = useMemo(
    () => (selected?.links ?? []).map((link) => link.label),
    [selected],
  );
  const rows = useExplorerRows({
    client: client ?? null,
    namespace: selected?.name ?? null,
    where,
    sort,
    limit: view.size,
    linkLabels,
  });

  const [selection, setSelection] = useState<string[]>([]);
  const [newNamespaceOpen, setNewNamespaceOpen] = useState(false);
  const [sheetNonce, setSheetNonce] = useState(0);

  // Selection belongs to one namespace; switching drops it.
  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect -- selection belongs to one namespace
    setSelection([]);
  }, [selected?.name]);

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
  const [hidden, setHidden] = useState<string[]>(() =>
    selected === null ? [] : readHiddenColumns(app.id, selected.name),
  );

  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect -- pins are stored per app in localStorage
    setPins(readNamespacePins(app.id));
  }, [app.id]);

  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect -- hidden columns are stored per app and namespace in localStorage
    setHidden(
      selected === null ? [] : readHiddenColumns(app.id, selected.name),
    );
  }, [app.id, selected]);

  const columns = useMemo(() => explorerColumns(selected), [selected]);
  const shown = useMemo(
    () => visibleColumns(columns, hidden),
    [columns, hidden],
  );

  const [searchDraft, setSearchDraft] = useState(view.text);
  const [searchError, setSearchError] = useState<string | null>(null);

  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect -- the URL is the source of truth for the search box
    setSearchDraft(view.text);
  }, [view.text]);

  const applyView = useCallback(
    (patch: Partial<ExplorerUrlState>, options?: { replace?: boolean }) => {
      onSearchChange(
        explorerSearchFor(
          {
            ...view,
            namespace: view.namespace ?? selected?.name ?? null,
            ...patch,
          },
          attributes,
        ),
        options,
      );
    },
    [attributes, onSearchChange, selected, view],
  );

  const openEntity = useCallback(
    (id: string) => {
      applyView({ entity: id, creating: false });
    },
    [applyView],
  );

  const openNewEntity = useCallback(() => {
    applyView({ entity: null, creating: true });
  }, [applyView]);

  const closeSheet = useCallback(() => {
    applyView({ entity: null, creating: false });
  }, [applyView]);

  const handleCellEdit = useCallback(
    (attribute: AppAttribute, row: ExplorerRow, value: unknown) => {
      if (selected === null || client === undefined) return;
      if (typeof row.id !== 'string') return;
      const parsed = parseCellValue(attribute, value, row[attribute.name]);
      if (!parsed.ok) {
        toastManager.add({ type: 'error', title: parsed.message });
        return;
      }
      void updateCell(
        client,
        selected.name,
        row.id,
        attribute.name,
        parsed.value,
      ).catch((failure: unknown) => {
        showErrorToast(failure);
      });
    },
    [client, selected],
  );

  const exportSelection = useCallback(() => {
    if (selected === null) return;
    const entities = rows.rows.filter(
      (row) => typeof row.id === 'string' && selection.includes(row.id),
    );
    const file = entityExport(selected.name, entities);
    downloadJson(file.filename, file.json);
  }, [rows.rows, selected, selection]);

  const deleteSelection = useCallback(async () => {
    if (selected === null || client === undefined) return;
    try {
      await deleteEntities(client, selected.name, selection);
      toastManager.add({
        type: 'success',
        title: `${formatCount(selection.length)} deleted`,
      });
      setSelection([]);
    } catch (failure) {
      showErrorToast(failure);
    }
  }, [client, selected, selection]);

  const openNamespace = useCallback(
    (name: string) => {
      onSearchChange(
        explorerSearchFor(
          {
            namespace: name,
            text: '',
            conditions: [],
            sort: null,
            size: view.size,
          },
          [],
        ),
      );
    },
    [onSearchChange, view.size],
  );

  const selectNamespace = useCallback(
    (name: string) => {
      const target = namespaces.find((candidate) => candidate.name === name);
      if (target === undefined) return;
      const targetAttributes: FilterAttribute[] = target.attributes.map(
        (attribute) => ({
          name: attribute.name,
          type: attribute.type,
          indexed: attribute.isIndexed,
        }),
      );
      setSearchError(null);
      onSearchChange(
        explorerSearchFor(
          {
            namespace: name,
            text: '',
            conditions: [],
            sort: null,
            size: view.size,
          },
          targetAttributes,
        ),
      );
    },
    [namespaces, onSearchChange, view],
  );

  const handleSearchChange = useCallback(
    (next: string) => {
      setSearchDraft(next);
      const parsed = parseSearchInput(next, attributes);
      if (parsed.kind === 'invalid') {
        setSearchError(parsed.message);
        return;
      }
      if (parsed.kind === 'filter') {
        setSearchDraft('');
        setSearchError(null);
        applyView({
          text: '',
          conditions: [...view.conditions, parsed.condition],
        });
        return;
      }
      if (
        next.trim() !== '' &&
        !attributes.some(
          (attribute) =>
            attribute.type === 'string' && attribute.indexed === true,
        )
      ) {
        setSearchError(
          'Search needs an indexed string attribute. Use Filter instead.',
        );
        return;
      }
      setSearchError(null);
      applyView({ text: next });
    },
    [applyView, attributes, view],
  );

  const clearFilters = useCallback(() => {
    setSearchDraft('');
    setSearchError(null);
    applyView({ text: '', conditions: [] });
  }, [applyView]);

  // `/` focuses the search box and Shift+R refreshes the list (B.7).
  useEffect(() => {
    const listener = (event: Event) => {
      const detail = (event as CustomEvent<{ action?: string } | undefined>)
        .detail;
      if (detail?.action === 'search') {
        const input = toolbarRef.current?.querySelector<HTMLInputElement>(
          'input[role="searchbox"]',
        );
        if (input === null || input === undefined) return;
        event.preventDefault();
        input.focus();
        return;
      }
      if (detail?.action === 'refresh') {
        event.preventDefault();
        rows.refresh();
        return;
      }
      if (detail?.action === 'new') {
        event.preventDefault();
        openNewEntity();
      }
    };
    window.addEventListener(PAGE_ACTION_EVENT, listener);
    return () => {
      window.removeEventListener(PAGE_ACTION_EVENT, listener);
    };
  }, [openNewEntity, rows]);

  const handleSortChange = useCallback(
    (next: ExplorerSort | null) => {
      if (next === null && sort === null) return;
      if (
        next !== null &&
        sort !== null &&
        next.attribute === sort.attribute &&
        next.direction === sort.direction
      ) {
        return;
      }
      applyView({ sort: next });
    },
    [applyView, sort],
  );

  const filtering = view.text.trim() !== '' || view.conditions.length > 0;
  const totalCount =
    counts[selected?.name ?? ''] ??
    (rows.hasNextPage ? undefined : rows.rows.length);
  const emptyNotice = filtering
    ? 'No results for these filters'
    : selected === null
      ? 'No namespaces yet'
      : `No entities in ${selected.name} yet`;
  const sheetEntityId = view.creating === true ? null : (view.entity ?? null);
  const sheetOpen = sheetEntityId !== null || view.creating === true;

  return (
    <div className="flex flex-col gap-3">
      <p className="text-muted-foreground text-xs md:hidden">
        The explorer works best on a larger screen.
      </p>

      {selected === null ? (
        schema.isLoading ? (
          <TableSkeleton rows={8} columns={4} />
        ) : (
          <EmptyState
            icon={Table2}
            title="No namespaces yet"
            description="Namespaces appear here once the app writes data."
            action={
              <Button
                type="button"
                onClick={() => {
                  setNewNamespaceOpen(true);
                }}
              >
                <Plus aria-hidden="true" />
                New namespace
              </Button>
            }
          />
        )
      ) : (
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
              selected={selected.name}
              pins={pins}
              isLoading={schema.isLoading}
              onSelect={selectNamespace}
              onTogglePin={(name) => {
                setPins(toggleNamespacePin(app.id, name));
              }}
              onNewNamespace={() => {
                setNewNamespaceOpen(true);
              }}
            />
          </ResizablePanel>
          <ResizableHandle />
          <ResizablePanel defaultSize="80%" className="min-w-0 pl-2">
            <div className="flex h-full flex-col gap-2">
              <div
                ref={toolbarRef}
                className="flex flex-wrap items-center gap-2"
              >
                <Button
                  type="button"
                  variant="secondary"
                  aria-label={
                    rows.pending ? 'Refresh (new changes available)' : 'Refresh'
                  }
                  onClick={() => {
                    rows.refresh();
                  }}
                >
                  <RefreshCw aria-hidden="true" />
                  Refresh
                  {rows.pending ? (
                    <span
                      aria-hidden="true"
                      className="bg-warning size-1.5 rounded-full"
                    />
                  ) : null}
                </Button>
                <SearchInput
                  aria-label="Search or filter"
                  placeholder="Search or filter…"
                  value={searchDraft}
                  onValueChange={handleSearchChange}
                  showShortcut
                  className="min-w-40 flex-1"
                />
                <FilterButton
                  conditions={view.conditions}
                  attributes={attributes}
                  onChange={(conditions) => {
                    applyView({ conditions });
                  }}
                />
                <ColumnsMenu
                  columns={columns}
                  hidden={hidden}
                  onToggle={(columnId) => {
                    setHidden(
                      toggleHiddenColumn(app.id, selected.name, columnId),
                    );
                  }}
                />
                <Button
                  type="button"
                  variant="secondary"
                  onClick={openNewEntity}
                >
                  <Plus aria-hidden="true" />
                  New entity
                </Button>
                {rows.paused ? (
                  <Button
                    type="button"
                    variant="secondary"
                    onClick={rows.resume}
                  >
                    <Play aria-hidden="true" />
                    Resume
                  </Button>
                ) : (
                  <Button
                    type="button"
                    variant="secondary"
                    aria-pressed={true}
                    onClick={rows.togglePause}
                  >
                    <Pause aria-hidden="true" />
                    Live
                  </Button>
                )}
                {searchError === null ? null : (
                  <p role="alert" className="text-destructive w-full text-xs">
                    {searchError}
                  </p>
                )}
                {rows.volumePaused ? (
                  <p role="status" className="text-warning w-full text-xs">
                    Live updates paused (high volume). Resume to follow them
                    again.
                  </p>
                ) : rows.pending && !rows.paused ? (
                  <p role="status" className="sr-only">
                    New changes available
                  </p>
                ) : null}
              </div>

              {rows.error !== null ? (
                <ErrorState
                  thing="entities"
                  message={rows.error}
                  onRetry={rows.refresh}
                />
              ) : (
                <>
                  {rows.rows.length === 0 && !rows.isLoading ? (
                    <p className="text-muted-foreground flex flex-wrap items-center gap-2 text-sm">
                      {emptyNotice}
                      {filtering ? (
                        <Button
                          type="button"
                          variant="link"
                          size="sm"
                          onClick={clearFilters}
                        >
                          Clear filters
                        </Button>
                      ) : null}
                    </p>
                  ) : null}

                  <div className="h-[60vh] min-h-[360px]">
                    <ExplorerGrid
                      appId={app.id}
                      columns={shown}
                      rows={rows.rows}
                      sort={sort}
                      loading={rows.isLoading}
                      onSortChange={handleSortChange}
                      onSelectionChange={setSelection}
                      onEditCell={handleCellEdit}
                      onOpenEntity={openEntity}
                    />
                  </div>
                </>
              )}

              <BulkActions
                count={selection.length}
                onExport={exportSelection}
                onDelete={deleteSelection}
                onClear={() => {
                  setSelection([]);
                }}
              />

              <div className="flex flex-wrap items-center justify-between gap-2">
                <p className="text-muted-foreground text-xs">
                  {`${entityCountLabel(totalCount, rows.rows.length)} entities · loaded ${formatCount(rows.rows.length)}`}
                </p>
                {rows.hasNextPage ? (
                  <Button
                    type="button"
                    variant="secondary"
                    size="sm"
                    disabled={rows.isLoading}
                    onClick={() => {
                      applyView(
                        {
                          size: Math.min(
                            view.size + EXPLORER_PAGE_STEP,
                            MAX_EXPLORER_PAGE_SIZE,
                          ),
                        },
                        { replace: true },
                      );
                    }}
                  >
                    Load more
                  </Button>
                ) : null}
              </div>
            </div>
          </ResizablePanel>
        </ResizablePanelGroup>
      )}

      {client === undefined ? null : (
        <>
          <NewNamespaceDialog
            open={newNamespaceOpen}
            onOpenChange={setNewNamespaceOpen}
            client={client}
            existing={namespaceNames}
            onCreated={openNamespace}
          />

          {sheetOpen && selected !== null ? (
            <EntitySheet
              key={`${app.id}:${selected.name}:${view.entity ?? 'new'}:${String(sheetNonce)}`}
              appId={app.id}
              namespace={selected}
              namespaces={namespaces}
              client={client}
              entityId={sheetEntityId}
              onOpenChange={(next) => {
                if (!next) closeSheet();
              }}
              onSaved={(_entityId, mode) => {
                if (mode === 'another') {
                  setSheetNonce((current) => current + 1);
                  openNewEntity();
                  return;
                }
                closeSheet();
              }}
            />
          ) : null}
        </>
      )}
    </div>
  );
}
