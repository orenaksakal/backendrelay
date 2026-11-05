import { createFileRoute } from '@tanstack/react-router';
import { DashHome } from '../routes';

export const Route = createFileRoute('/dash')({
  component: DashHome,
  ssr: false,
});
