import { PageHeader } from '@/components/layout/page-header';
import { CreateAppForm } from '@/features/creation/create-app-form';

/** C.3 first run: the same create form without the org dialog. */
export function WelcomePage() {
  return (
    <div className="mx-auto flex w-full max-w-[480px] flex-col gap-4">
      <PageHeader
        title="Create your first app"
        description="Name your app and choose where it lives. You can change both later."
      />
      <CreateAppForm />
    </div>
  );
}
