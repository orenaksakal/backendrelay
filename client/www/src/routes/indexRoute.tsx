import { createFileRoute } from '@tanstack/react-router';
import { DashboardRedirect } from '../routes';

export const Route = createFileRoute('/')({
  component: DashboardRedirect,
  ssr: false,
});
