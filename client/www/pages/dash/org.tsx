import { useReadyRouter } from '@/components/clientOnlyPage';
import { BackToAppsButton } from '@/components/dash/BackToAppsButton';
import { useFetchedDash } from '@/components/dash/MainDashLayout';
import { Members } from '@/components/dash/org-management/Members';
import { OrgManagePage } from '@/components/dash/org-management/OrgManagePage';
import { NavTabBar, SmallCopyable } from '@/components/ui';
import { BuildingOffice2Icon } from '@heroicons/react/24/outline';
import Head from '@/components/PageHead';
import { parseAsStringEnum, useQueryState } from 'nuqs';
import { isMinRole, Role } from '.';
import { capitalize } from 'lodash';
import useLocalStorage from '@/lib/hooks/useLocalStorage';

const OrgSettingsPage = () => {
  const fetchedDash = useFetchedDash();
  const router = useReadyRouter();

  const [tab, setTab] = useQueryState(
    'tab',
    parseAsStringEnum(['members', 'manage']).withDefault('members'),
  );
  const org = fetchedDash.data.workspace;

  const [hideOrgId, setHideOrgId] = useLocalStorage('hide_org_id', false);

  if (
    fetchedDash.data.currentWorkspaceId === 'personal' ||
    org.type === 'personal'
  ) {
    router.replace('/dash');
    return;
  }

  const myRole = org.org.role as Role;

  if (!myRole) {
    throw new Error('User not found in organization');
  }

  let tabs = [{ id: 'members', label: 'Members' }];
  if (isMinRole('admin', myRole)) {
    tabs.push({ id: 'manage', label: 'Manage' });
  }

  return (
    <>
      <Head>
        <title>Organization Settings</title>
      </Head>
      <BackToAppsButton />
      <div className="bg-gray-50 dark:bg-neutral-800">
        <div className="flex flex-col justify-between border-b border-b-gray-300 px-3 py-2 md:flex-row md:gap-4 dark:border-b-neutral-800">
          <div className="flex items-center gap-2">
            <h2 className="font-mono font-bold md:text-xl">
              <div className="flex items-center gap-4">
                <BuildingOffice2Icon className="opacity-40" width={20} />
                <div className="text-lg font-bold">{org.org.title}</div>
              </div>
            </h2>
          </div>
          <SmallCopyable
            size="normal"
            label="Org ID"
            value={org.org.id}
            hideValue={hideOrgId}
            onChangeHideValue={() => {
              setHideOrgId(!hideOrgId);
            }}
          />
        </div>
      </div>

      <div className="mx-auto w-full max-w-[680px] overflow-scroll px-4 pt-6 lg:px-12">
        <NavTabBar
          className="border-transparent"
          tabs={tabs}
          selectedId={tab}
          onSelect={(t) => setTab(t.id as 'members' | 'manage')}
        />
        {tab === 'members' && <Members />}
        {tab === 'manage' && isMinRole('admin', myRole) && <OrgManagePage />}
      </div>
    </>
  );
};

export default OrgSettingsPage;
