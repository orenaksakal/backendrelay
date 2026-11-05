import { describe, expect, it } from 'vitest';
import { ApiError } from './api/client';
import {
  QUERY_STALE_TIME_MS,
  createQueryClient,
  shouldRetryQuery,
} from './query-client';

describe('query client defaults', () => {
  it('keeps queries fresh for 30 seconds', () => {
    const client = createQueryClient();

    expect(QUERY_STALE_TIME_MS).toBe(30_000);
    expect(client.getDefaultOptions().queries?.staleTime).toBe(
      QUERY_STALE_TIME_MS,
    );
  });

  it('does not retry 4xx responses', () => {
    const error = new ApiError({
      status: 404,
      type: 'record-not-found',
      message: 'missing',
    });

    expect(shouldRetryQuery(0, error)).toBe(false);
  });

  it('retries transient failures once', () => {
    const unavailable = new ApiError({
      status: 503,
      type: 'unknown',
      message: 'down',
    });
    const timeout = new ApiError({
      status: 0,
      type: 'timeout',
      message: 'slow',
    });

    expect(shouldRetryQuery(0, unavailable)).toBe(true);
    expect(shouldRetryQuery(1, unavailable)).toBe(false);
    expect(shouldRetryQuery(0, timeout)).toBe(false);
    expect(shouldRetryQuery(0, new Error('boom'))).toBe(true);
  });
});
