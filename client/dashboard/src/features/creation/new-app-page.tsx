import { PageHeader } from '@/components/layout/page-header';
import { CreateAppForm } from '@/features/creation/create-app-form';

export interface NewAppPageProps {
  workspaceId?: string;
}

/** C.3 `/new`: app creation plus the "Create an org instead" dialog. */
export function NewAppPage({ workspaceId }: NewAppPageProps) {
  return (
    <div className="mx-auto flex w-full max-w-[480px] flex-col gap-4">
      <PageHeader
        title="Create app"
        description="Name your app and choose the workspace it belongs to."
      />
      <CreateAppForm initialWorkspaceId={workspaceId} showCreateOrg />
    </div>
  );
}
