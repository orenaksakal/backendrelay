import { z } from 'zod';
import { restoreJobSchema } from './backups';
import { defineEndpoint } from './types';

const knownFlagSchema = z.looseObject({
  name: z.string(),
  type: z.string(),
  description: z.string(),
});

export type KnownOperatorFlag = z.infer<typeof knownFlagSchema>;

const flagsResponseSchema = z.looseObject({
  flags: z.record(z.string(), z.unknown()),
  known: z.array(knownFlagSchema),
});

export type OperatorFlags = z.infer<typeof flagsResponseSchema>;

/** Operator settings and restores (C.19, D0.3). */
export const operatorEndpoints = {
  flags: defineEndpoint({
    method: 'GET',
    path: '/dash/operator/flags',
    response: flagsResponseSchema,
  }),

  setFlag: defineEndpoint({
    method: 'PUT',
    path: '/dash/operator/flags/:name',
    body: z.looseObject({ value: z.unknown() }),
    response: z.looseObject({ name: z.string(), value: z.unknown() }),
  }),

  deleteFlag: defineEndpoint({
    method: 'DELETE',
    path: '/dash/operator/flags/:name',
    response: z.looseObject({ name: z.string(), deleted: z.boolean() }),
  }),

  restoreJobs: defineEndpoint({
    method: 'GET',
    path: '/dash/restore-jobs',
    response: z.looseObject({ 'restore-jobs': z.array(restoreJobSchema) }),
  }),

  restoreJob: defineEndpoint({
    method: 'GET',
    path: '/dash/restore-jobs/:jobId',
    response: z.looseObject({
      'restore-job': restoreJobSchema.nullable(),
    }),
  }),

  cancelRestoreJob: defineEndpoint({
    method: 'DELETE',
    path: '/dash/restore-jobs/:jobId',
    response: z.looseObject({
      'restore-job': restoreJobSchema.nullable(),
    }),
  }),
};
