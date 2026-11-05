import { AppStatus, InstantApp } from './types';

export function displayAppStatus(
  app: Pick<InstantApp, 'status' | 'effective_status'>,
): AppStatus {
  if (app.effective_status) return app.effective_status;
  return app.status ?? 'active';
}
