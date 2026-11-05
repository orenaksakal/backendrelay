import { jsonFetch } from '@/lib/fetch';

type DataRow = { id: string; [key: string]: unknown };

function filePathsForDeletion(rows: DataRow[], ids: string[]): string[] {
  const byId = new Map(rows.map((row) => [row.id, row]));
  return ids.map((id) => {
    const path = byId.get(id)?.path;
    if (typeof path !== 'string' || !path.trim()) {
      throw new Error(
        'A selected file is no longer loaded. Refresh and select it again.',
      );
    }
    return path;
  });
}

export function explorerStorageHandlers(
  apiURI: string,
  appId: string,
  token: string,
  request: (url: string, init: RequestInit) => Promise<unknown> = jsonFetch,
) {
  const base = `${apiURI}/dash/apps/${encodeURIComponent(appId)}/storage`;
  return {
    async onUploadFile(file: File, path: string) {
      await request(`${base}/upload?path=${encodeURIComponent(path)}`, {
        method: 'PUT',
        headers: {
          authorization: `Bearer ${token}`,
          'content-type': file.type || 'application/octet-stream',
          ...(/^[\x20-\x7e]*$/.test(path) ? { path } : {}),
        },
        body: file,
      });
    },
    async onDeleteFiles(rows: DataRow[], ids: string[]) {
      const filenames = filePathsForDeletion(rows, ids);
      await request(`${base}/files/delete`, {
        method: 'POST',
        headers: {
          authorization: `Bearer ${token}`,
          'content-type': 'application/json',
        },
        body: JSON.stringify({ filenames }),
      });
    },
  };
}
