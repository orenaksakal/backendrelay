import { createFileRoute } from '@tanstack/react-router';
import { DeploymentSettings } from '../routes';

export const Route = createFileRoute('/intern/deployment-settings')({
  component: DeploymentSettings,
  ssr: false,
});
