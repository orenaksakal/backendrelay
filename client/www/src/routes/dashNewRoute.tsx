import { createFileRoute } from '@tanstack/react-router';
import { DashNew } from '../routes';

export const Route = createFileRoute('/dash/new')({
  component: DashNew,
  ssr: false,
});
