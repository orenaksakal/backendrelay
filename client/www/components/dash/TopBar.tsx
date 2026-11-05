import {
  ArrowTopRightOnSquareIcon,
  EnvelopeIcon,
} from '@heroicons/react/24/outline';
import { PlusIcon } from '@heroicons/react/24/solid';
import Link from '@/lib/navigation';
import { Button } from '@/components/ui';
import { useFetchedDash } from './MainDashLayout';
import { DarkModeToggle } from './DarkModeToggle';
import { workspaceHref } from './shell/model';
import { ProfilePanel } from './ProfilePanel';

export const TopBar: React.FC<{}> = () => {
  const dash = useFetchedDash();
  // Use the workspace from the dash context
  const orgId =
    dash.data?.currentWorkspaceId !== 'personal'
      ? dash.data?.currentWorkspaceId
      : null;
  const docsUrl = orgId ? `/docs?org=${orgId}` : '/docs';

  const hasInvites = (dash.data.invites || []).length > 0;

  return (
    <header className="dashboard-topbar relative flex shrink-0 flex-col gap-2 border-b border-b-gray-300 px-2 py-2 md:px-4 dark:border-b-neutral-700 dark:bg-neutral-800 dark:text-white">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex min-w-0 flex-row items-center gap-2">
          <ProfilePanel />
          <div id="left-top-bar" className="min-w-0" />
        </div>
        <nav
          className="flex items-center gap-4 md:gap-6"
          aria-label="Dashboard actions"
        >
          {hasInvites && (
            <Link
              className="ml-3"
              href={workspaceHref(
                '/dash/user-settings',
                dash.data.currentWorkspaceId,
                { tab: 'invites' },
              )}
            >
              <span className="flex animate-bounce gap-2 text-sm">
                <EnvelopeIcon width={14} />
                Pending Invites ({dash.data.invites?.length ?? 0})
              </span>
            </Link>
          )}
          <Link
            target="_blank"
            rel="noreferrer"
            className="flex items-center gap-1 text-sm opacity-50 hover:underline"
            href={docsUrl}
          >
            Docs
            <ArrowTopRightOnSquareIcon className="h-4 w-4" />
          </Link>
          <DarkModeToggle />
          <Link
            href={workspaceHref('/dash/new', dash.data.currentWorkspaceId)}
            className="inline-flex"
          >
            <Button size="mini" variant="primary">
              <PlusIcon height={14} />
              New app
            </Button>
          </Link>
        </nav>
      </div>
    </header>
  );
};
