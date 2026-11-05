import { History as HistoryIcon } from 'lucide-react';
import { useState } from 'react';
import { RelativeTime } from '@/components/data/relative-time';
import {
  SimpleTable,
  type SimpleTableColumn,
} from '@/components/data/simple-table';
import { ConfirmDialog } from '@/components/feedback/confirm-dialog';
import { EmptyState } from '@/components/feedback/empty-state';
import { ErrorState } from '@/components/feedback/error-state';
import { TableSkeleton } from '@/components/feedback/skeletons';
import { SectionCard } from '@/components/layout/section-card';
import { Button } from '@/components/ui/button';
import { isApiError } from '@/lib/api/client';
import {
  reconstructVersion,
  rulesJson,
  sortedVersions,
  versionsInSync,
  type RuleVersion,
  type Rules,
} from '@/lib/permissions/model';
import { ruleVersionsFrom, useRuleVersions } from '@/lib/permissions/queries';
import { PermissionsDiff } from './diff-view';

export interface PermissionsHistoryViewProps {
  appId: string;
  rules: unknown;
  rulesVersion: number | null | undefined;
  enabled: boolean;
  /** Loads a version into the shared draft; saving still applies it. */
  onRestore: (rules: Rules) => void;
}

/**
 * C.10 History: the last 50 server-recorded versions. Selecting one diffs it
 * against the live rules; restoring only fills the draft, so the save dialog
 * and its diff still stand between a click and live permissions.
 */
export function PermissionsHistoryView({
  appId,
  rules,
  rulesVersion,
  enabled,
  onRestore,
}: PermissionsHistoryViewProps) {
  const query = useRuleVersions(appId, enabled);
  const [selected, setSelected] = useState<number | null>(null);
  const [restoreOpen, setRestoreOpen] = useState(false);

  if (query.isPending) return <TableSkeleton rows={4} columns={2} />;

  if (query.isError) {
    return (
      <ErrorState
        thing="permission history"
        status={isApiError(query.error) ? query.error.status : undefined}
        message={query.error instanceof Error ? query.error.message : undefined}
        type={isApiError(query.error) ? query.error.type : undefined}
        hint={isApiError(query.error) ? query.error.hint : undefined}
        requestId={isApiError(query.error) ? query.error.requestId : undefined}
        onRetry={() => {
          void query.refetch();
        }}
      />
    );
  }

  const versions = sortedVersions(ruleVersionsFrom(query.data.versions));
  if (versions.length === 0) {
    return (
      <EmptyState
        icon={HistoryIcon}
        title="No saved versions yet"
        description="Save the permission rules once and every later save appears here."
      />
    );
  }

  const inSync = versionsInSync(rulesVersion, versions);
  const currentRules: Rules | null =
    rules !== null && typeof rules === 'object' && !Array.isArray(rules)
      ? (rules as Rules)
      : null;
  const reconstruction =
    inSync && selected !== null && currentRules !== null
      ? reconstructVersion(currentRules, rulesVersion, versions, selected)
      : null;

  const columns: SimpleTableColumn<RuleVersion>[] = [
    {
      id: 'version',
      header: 'Version',
      cell: (version) => (
        <Button
          type="button"
          variant="link"
          size="sm"
          aria-label={`View version ${String(version.version)}`}
          onClick={() => {
            setSelected(version.version);
          }}
        >
          v{version.version}
        </Button>
      ),
    },
    {
      id: 'created',
      header: 'Saved',
      cell: (version) => <RelativeTime value={version.created_at} />,
    },
  ];

  return (
    <div className="flex flex-col gap-4">
      {inSync ? null : (
        <p role="status" className="text-warning text-sm">
          Version history is out of sync with the current rules. Save the rules
          to start a new version.
        </p>
      )}

      <SimpleTable
        caption="Rule versions"
        columns={columns}
        rows={versions}
        getRowId={(version) => String(version.version)}
        onRowActivate={(version) => {
          setSelected(version.version);
        }}
      />

      {selected === null ? (
        <p className="text-muted-foreground text-sm">
          Select a version to compare it with the current rules.
        </p>
      ) : (
        <SectionCard
          title={`Version ${String(selected)}`}
          description="A restore loads this document into the draft; nothing is saved until you save."
          actions={
            <Button
              type="button"
              variant="secondary"
              size="sm"
              disabled={reconstruction === null}
              onClick={() => {
                setRestoreOpen(true);
              }}
            >
              Restore this version
            </Button>
          }
        >
          {reconstruction === null || currentRules === null ? (
            <p className="text-muted-foreground text-sm">
              Version history unavailable for this selection.
            </p>
          ) : (
            <PermissionsDiff
              before={rulesJson(currentRules)}
              after={rulesJson(reconstruction.rules)}
              label={`Current → v${String(selected)}`}
            />
          )}
        </SectionCard>
      )}

      <ConfirmDialog
        open={restoreOpen && reconstruction !== null}
        onOpenChange={setRestoreOpen}
        title={`Restore version ${String(selected ?? '')}?`}
        description="This replaces the draft with the selected version. Review the diff, then save to apply it."
        confirmLabel="Restore this version"
        onConfirm={() => {
          if (reconstruction !== null) onRestore(reconstruction.rules);
          setRestoreOpen(false);
        }}
      />
    </div>
  );
}
