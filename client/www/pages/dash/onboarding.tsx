import { asClientOnlyPage } from '@/components/clientOnlyPage';
import { Onboarding } from '@/components/dash/Onboarding';

export const OnboardingPage = () => {
  return <Onboarding />;
};

const Page = asClientOnlyPage(OnboardingPage);

export default Page;
