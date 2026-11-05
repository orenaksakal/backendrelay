import { createFileRoute } from '@tanstack/react-router';
import { DashUserSettings } from '../routes';

export const Route = createFileRoute('/dash/user-settings')({
  component: DashUserSettings,
  ssr: false,
});
