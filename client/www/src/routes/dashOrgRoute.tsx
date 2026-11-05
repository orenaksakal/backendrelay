import { createFileRoute } from '@tanstack/react-router';
import { DashOrganization } from '../routes';

export const Route = createFileRoute('/dash/org')({
  component: DashOrganization,
  ssr: false,
});
