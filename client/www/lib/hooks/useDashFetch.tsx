import { APIResponse, useAuthedFetch } from '../auth';
import config from '../config';
import { DashResponse } from '../types';

export type CachedAPIResponse<T> = APIResponse<T> & { fromCache?: boolean };
export function useDashFetch(): CachedAPIResponse<DashResponse> {
  return useAuthedFetch<DashResponse>(`${config.apiURI}/dash`);
}
