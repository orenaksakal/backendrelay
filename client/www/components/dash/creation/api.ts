import config from '@/lib/config';
import { jsonFetch } from '@/lib/fetch';
export function createApp(
  token: string,
  toCreate: {
    id: string;
    title: string;
    admin_token: string;
    org_id?: string | null;
  },
) {
  return jsonFetch(`${config.apiURI}/dash/apps`, {
    method: 'POST',
    headers: {
      authorization: `Bearer ${token}`,
      'content-type': 'application/json',
    },
    body: JSON.stringify(toCreate),
  });
}
