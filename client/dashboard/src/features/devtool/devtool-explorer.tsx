import { Pause, Play, RefreshCw, Table2 } from 'lucide-react';
import { useMemo, useState } from 'react';
import { TableSkeleton } from '@/components/feedback/skeletons';
import { ErrorState } from '@/components/feedback/error-state';
import { AppDataGate } from '@/components/data/app-data-gate';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { IconButton } from '@/components/layout/icon-button';
import type { InstaqlWhere } from '@/components/data/filter-serialization';
import type { App, EffectiveRole } from '@/lib/api/endpoints/schemas';
import { explorerColumns } from '@/lib/explorer/columns';
import { entityCountLabel } from '@/lib/explorer/entities';
import { useExplorerRows, useNamespaceCounts } from '@/lib/explorer/rows';
import type { ExplorerSort } from '@/lib/explorer/url-state';
import { peekAppClient, useAppSchema } from '@/lib/instant';
import { ExplorerGrid } from '@/features/explorer/explorer-grid';

export interface DevtoolExplorerProps {
  app: App;
  role: EffectiveRole;
}

const NO_WHERE: InstaqlWhere = {};
const DEVTOOL_PAGE_SIZE = 50;

/**
 * C.20 Explorer tab: the C.6 grid at 400px, with the namespace sidebar
 * replaced by a select. The grid is read-only because edit sheets need the
 * full dashboard shell; inspecting data and running queries stay here.
 */
export function DevtoolExplorer({ app, role }: DevtoolExplorerProps) {
  return (
    <AppDataGate app={app} role={role} thing="explorer">
      <DevtoolExplorerData appId={app.id} />
    </AppDataGate>
  );
}

function DevtoolExplorerData({ appId }: { appId: string }) {
  const client = peekAppClient(appId);
  const schema = useAppSchema(appId);
  const [namespaceName, setNamespaceName] = useState<string | null>(null);
  const [sort, setSort] = useState<ExplorerSort | null>(null);

  const selected = useMemo(() => {
    const explicit = schema.namespaces.find(
      (namespace) => namespace.name === namespaceName,
    );
    return (
      explicit ??
      schema.namespaces.find((namespace) => !namespace.isSystem) ??
      schema.namespaces[0] ??
      null
    );
  }, [namespaceName, schema.namespaces]);

  const columns = useMemo(() => explorerColumns(selected), [selected]);
  const linkLabels = useMemo(
    () => (selected?.links ?? []).map((link) => link.label),
    [selected],
  );
  const namespaceNames = useMemo(
    () => schema.namespaces.map((namespace) => namespace.name),
    [schema.namespaces],
  );
  const counts = useNamespaceCounts(client ?? null, namespaceNames);
  const rows = useExplorerRows({
    client: client ?? null,
    namespace: selected?.name ?? null,
    where: NO_WHERE,
    sort,
    limit: DEVTOOL_PAGE_SIZE,
    linkLabels,
  });

  if (schema.isLoading) {
    return (
      <div className="flex flex-col gap-2">
        <p role="status" className="sr-only">
          Loading namespaces…
        </p>
        <TableSkeleton rows={5} columns={3} />
      </div>
    );
  }

  if (selected === null) {
    return (
      <div className="flex flex-col items-center gap-2 p-6 text-center">
        <Table2 aria-hidden="true" className="text-muted-foreground size-6" />
        <p className="text-sm font-medium">No namespaces yet</p>
        <p className="text-muted-foreground text-xs">
          Namespaces appear here once the app writes data.
        </p>
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-0 flex-col gap-2">
      <div className="flex items-center gap-2">
        <Select
          value={selected.name}
          onValueChange={(value) => {
            if (typeof value !== 'string') return;
            setNamespaceName(value);
            setSort(null);
          }}
        >
          <SelectTrigger aria-label="Namespace" className="min-w-0 flex-1">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {schema.namespaces.map((namespace) => (
              <SelectItem key={namespace.name} value={namespace.name}>
                {namespace.name}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <IconButton
          aria-label="Refresh"
          icon={<RefreshCw aria-hidden="true" />}
          size="sm"
          onClick={() => {
            rows.refresh();
          }}
        />
        {rows.paused ? (
          <IconButton
            aria-label="Resume live updates"
            icon={<Play aria-hidden="true" />}
            size="sm"
            onClick={rows.resume}
          />
        ) : (
          <IconButton
            aria-label="Pause live updates"
            icon={<Pause aria-hidden="true" />}
            size="sm"
            onClick={rows.togglePause}
          />
        )}
      </div>

      {rows.error !== null ? (
        <ErrorState
          thing="entities"
          message={rows.error}
          onRetry={rows.refresh}
        />
      ) : (
        <>
          <div className="min-h-0 flex-1">
            <ExplorerGrid
              appId={appId}
              columns={columns}
              rows={rows.rows}
              sort={sort}
              loading={rows.isLoading}
              readOnly
              onSortChange={setSort}
              onSelectionChange={() => undefined}
              onEditCell={() => undefined}
              onOpenEntity={() => undefined}
            />
          </div>
          <p className="text-muted-foreground text-xs">
            {`${entityCountLabel(
              counts[selected.name],
              rows.rows.length,
            )} entities · loaded ${String(rows.rows.length)}`}
          </p>
        </>
      )}
    </div>
  );
}
