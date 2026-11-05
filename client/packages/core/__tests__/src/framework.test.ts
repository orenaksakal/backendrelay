import { afterEach, describe, expect, it, vi } from 'vitest';
import { FrameworkClient } from '../../src/framework.ts';
import { InstantAPIError } from '../../src/index.ts';

function createClient() {
  return new FrameworkClient({
    db: {
      _reactor: {
        config: {
          apiURI: 'https://api.example.com',
          appId: 'app-id',
        },
      },
    } as any,
  });
}

function response(body: unknown, status = 200) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

describe('FrameworkClient.getTriplesAndAttrsForQuery', () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('parses a complete successful framework query response', async () => {
    const attrs = [{ id: 'attr-id' }];
    const triples = [['entity-id', 'attr-id', 'value', 1]];
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue(
        response({
          attrs,
          result: [
            {
              data: {
                'datalog-result': { 'join-rows': [triples] },
                'page-info': { 'end-cursor': 'cursor' },
              },
            },
          ],
        }),
      ),
    );

    await expect(
      createClient().getTriplesAndAttrsForQuery({ goals: {} }),
    ).resolves.toMatchObject({
      attrs,
      triples,
      pageInfo: { 'end-cursor': 'cursor' },
      type: 'http',
    });
  });

  it('rejects successful responses missing required query data', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue(response({ attrs: [], result: [] })),
    );

    await expect(
      createClient().getTriplesAndAttrsForQuery({ goals: {} }),
    ).rejects.toMatchObject({
      message: 'Error getting triples from framework client',
      cause: expect.objectContaining({
        message: 'Invalid framework query response: expected result entry',
      }),
    });
  });

  it.each([
    [
      'attributes',
      { attrs: ['bad'], result: [] },
      'Invalid framework query response: expected attrs array',
    ],
    [
      'triples',
      {
        attrs: [],
        result: [{ data: { 'datalog-result': { 'join-rows': [[['bad']]] } } }],
      },
      'Invalid framework query response: expected triples',
    ],
    [
      'page info',
      {
        attrs: [],
        result: [
          {
            data: {
              'datalog-result': { 'join-rows': [[]] },
              'page-info': [],
            },
          },
        ],
      },
      'Invalid framework query response: expected page info',
    ],
  ])('rejects malformed %s', async (_label, body, message) => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(response(body)));

    await expect(
      createClient().getTriplesAndAttrsForQuery({ goals: {} }),
    ).rejects.toMatchObject({
      cause: expect.objectContaining({ message }),
    });
  });

  it('preserves InstantAPIError for unsuccessful API responses', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue(response({ message: 'Access denied' }, 403)),
    );

    const error = await createClient()
      .getTriplesAndAttrsForQuery({ goals: {} })
      .catch((error) => error);

    expect(error).toBeInstanceOf(InstantAPIError);
    expect(error).toMatchObject({
      name: 'InstantAPIError',
      message: 'Access denied',
      status: 403,
      body: { message: 'Access denied' },
    });
  });
});
