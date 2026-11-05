import { ErrorState } from '@/components/feedback/error-state';
import { PageHeader } from '@/components/layout/page-header';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import type { App, EffectiveRole } from '@/lib/api/endpoints/schemas';
import {
  BACKUP_TABS,
  BACKUP_TAB_LABELS,
  canManageBackupSchedule,
  canViewBackups,
  isBackupTab,
  type BackupTab,
} from '@/lib/backups/model';
import { BackupJobsTab } from './backup-jobs-tab';
import { BackupScheduleTab } from './backup-schedule-tab';
import { SnapshotsTab } from './snapshots-tab';

export interface BackupsPageProps {
  app: App;
  role: EffectiveRole;
  tab: BackupTab;
  onTabChange: (tab: BackupTab) => void;
}

/**
 * C.15 backups: snapshots, the automatic schedule and the job list for one
 * app. The active tab lives in `?tab=`.
 */
export function BackupsPage({ app, role, tab, onTabChange }: BackupsPageProps) {
  if (!canViewBackups(role)) {
    return (
      <ErrorState
        thing="backups"
        status={403}
        requiredRole="collaborator"
        message="Only app collaborators, administrators and owners can see backups."
      />
    );
  }

  return (
    <div className="flex max-w-5xl flex-col gap-4">
      <Tabs
        value={tab}
        className="flex flex-col gap-4"
        onValueChange={(value) => {
          if (isBackupTab(value)) onTabChange(value);
        }}
      >
        <PageHeader
          title="Backups"
          description="Snapshots of this app's data and files, on demand or on a schedule."
          tabs={
            <TabsList aria-label="Backups">
              {BACKUP_TABS.map((value) => (
                <TabsTrigger key={value} value={value}>
                  {BACKUP_TAB_LABELS[value]}
                </TabsTrigger>
              ))}
            </TabsList>
          }
        />

        <TabsContent value="snapshots">
          <SnapshotsTab app={app} role={role} />
        </TabsContent>

        <TabsContent value="schedule">
          <BackupScheduleTab
            appId={app.id}
            canEdit={canManageBackupSchedule(role)}
          />
        </TabsContent>

        <TabsContent value="jobs">
          <BackupJobsTab appId={app.id} />
        </TabsContent>
      </Tabs>
    </div>
  );
}
