import { z } from 'zod';
import { defineEndpoint } from './types';

/** File uploads and deletes for the Storage screen (C.9). */
export const storageEndpoints = {
  upload: defineEndpoint({
    method: 'PUT',
    path: '/dash/apps/:appId/storage/upload',
    /** Sent as the raw request body; `path` is the query parameter. */
    body: z.instanceof(Blob),
    response: z.looseObject({ data: z.unknown() }),
  }),

  deleteFiles: defineEndpoint({
    method: 'POST',
    path: '/dash/apps/:appId/storage/files/delete',
    body: z.looseObject({ filenames: z.array(z.string()) }),
    response: z.looseObject({ data: z.unknown() }),
  }),
};
