import { createFileRoute } from '@tanstack/react-router';
import { DashOnboarding } from '../routes';

export const Route = createFileRoute('/dash/onboarding')({
  component: DashOnboarding,
  ssr: false,
});
