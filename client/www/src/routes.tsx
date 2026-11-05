import { useEffect, type ReactNode } from 'react';
import { lazyRouteComponent } from '@tanstack/react-router';
import { ClientOnly } from '@/components/clientOnlyPage';
import { MainDashLayout } from '@/components/dash/MainDashLayout';
import { useRouter } from '@/lib/navigation';

const NotFound = lazyRouteComponent(() => import('@/pages/404'));
const Dash = lazyRouteComponent(() => import('@/pages/dash'));
const NewApp = lazyRouteComponent(() => import('@/pages/dash/new'));
const Onboarding = lazyRouteComponent(() => import('@/pages/dash/onboarding'));
const Organization = lazyRouteComponent(() => import('@/pages/dash/org'));
const UserSettings = lazyRouteComponent(
  () => import('@/pages/dash/user-settings'),
);
const OAuthCallback = lazyRouteComponent(
  () => import('@/pages/dash/oauth/callback'),
);
const Devtool = lazyRouteComponent(() => import('@/pages/_devtool'));
const PlatformOAuthStart = lazyRouteComponent(
  () => import('@/pages/platform/oauth/start'),
);
const DeploymentSettings = lazyRouteComponent(
  () => import('@/pages/intern/deployment-settings'),
);

function DashboardLayout({ children }: { children: ReactNode }) {
  return <ClientOnly>{children}</ClientOnly>;
}

function DashHome() {
  return (
    <DashboardLayout>
      <MainDashLayout className="bg-ui-canvas text-ui-ink">
        <Dash />
      </MainDashLayout>
    </DashboardLayout>
  );
}

function DashNew() {
  return (
    <DashboardLayout>
      <MainDashLayout>
        <NewApp />
      </MainDashLayout>
    </DashboardLayout>
  );
}

function DashOnboarding() {
  return (
    <DashboardLayout>
      <MainDashLayout>
        <Onboarding />
      </MainDashLayout>
    </DashboardLayout>
  );
}

function DashOrganization() {
  return (
    <DashboardLayout>
      <MainDashLayout className="bg-ui-canvas text-ui-ink">
        <Organization />
      </MainDashLayout>
    </DashboardLayout>
  );
}

function DashUserSettings() {
  return (
    <DashboardLayout>
      <MainDashLayout className="bg-ui-canvas text-ui-ink">
        <UserSettings />
      </MainDashLayout>
    </DashboardLayout>
  );
}

function DashboardRedirect() {
  const router = useRouter();
  useEffect(() => {
    router.replace('/dash');
  }, [router]);
  return null;
}

export {
  DashHome,
  DashNew,
  DashOnboarding,
  DashOrganization,
  DashUserSettings,
  DashboardRedirect,
  DeploymentSettings,
  Devtool,
  NotFound,
  OAuthCallback,
  PlatformOAuthStart,
};
