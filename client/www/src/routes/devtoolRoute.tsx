import { createFileRoute } from '@tanstack/react-router';
import { Devtool } from '../routes';

export const Route = createFileRoute('/_devtool')({
  component: Devtool,
  ssr: false,
});
