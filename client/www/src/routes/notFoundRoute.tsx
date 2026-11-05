import { createFileRoute } from '@tanstack/react-router';
import { NotFound } from '../routes';

export const Route = createFileRoute('/$')({ component: NotFound, ssr: false });
