import { useFetchedDash } from '@/components/dash/MainDashLayout';
import { parseAsStringEnum, useQueryState } from 'nuqs';
import { Button, NavTabBar, TabItem } from '@/components/ui';
import PersonalAccessTokensTab from '@/components/dash/PersonalAccessTokensScreen';
import OAuthAppsTab from '@/components/dash/AuthorizedOAuthAppsScreen';
import { useRouter } from '@/lib/navigation';
import { signOut } from '@/lib/auth';
import { BackToAppsButton } from '@/components/dash/BackToAppsButton';
import { Invites } from '@/components/dash/Invites';
import Head from '@/components/PageHead';

const UserSettingsPage = () => {
  const [tab, setTab] = useQueryState(
    'tab',
    parseAsStringEnum(['token', 'oauth', 'invites']).withDefault('token'),
  );
  const dashResponse = useFetchedDash();
  const router = useRouter();

  const tabs: TabItem[] = [
    { id: 'token', label: 'Access Tokens' },
    { id: 'oauth', label: 'OAuth Apps' },
    { id: 'invites', label: 'Invites' },
  ];

  return (
    <>
      <Head>
        <title>User Settings</title>
      </Head>
      <BackToAppsButton />
      <div className="mx-auto w-full px-8 pt-8 md:max-w-4xl dark:text-white">
        <div className="flex items-center justify-between">
          <div className="flex flex-col items-baseline pb-4 md:flex-row md:gap-4">
            <div className="text-lg font-semibold">User Settings</div>
            <div className="text-gray-600 dark:text-neutral-400">
              {dashResponse.data.user.email}
            </div>
          </div>
          <Button
            onClick={async () => {
              await signOut();
              router.replace('/dash');
            }}
            variant="destructive"
          >
            Log Out
          </Button>
        </div>
        <NavTabBar
          className="border-transparent"
          tabs={tabs}
          selectedId={tab}
          onSelect={(t) => setTab(t.id as 'token' | 'oauth' | 'invites')}
        />
        <div className="mt-2">
          {tab === 'token' ? (
            <PersonalAccessTokensTab />
          ) : tab === 'oauth' ? (
            <OAuthAppsTab />
          ) : (
            <Invites />
          )}
        </div>
      </div>
    </>
  );
};

export default UserSettingsPage;
