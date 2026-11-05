import { describe, expect, test } from 'vitest';

import { init } from '../../src/index.ts';
import Reactor from '../../src/Reactor';
import { InstantError } from '../../src/InstantError.ts';
import {
  deriveWebsocketURI,
  MissingApiURIError,
  requireApiURI,
} from '../../src/urls.ts';
import id from '../../src/utils/id.ts';

const apiURI = 'https://api.example.com';

describe('backend URL configuration', () => {
  test('init requires an apiURI instead of defaulting to instantdb.com', () => {
    expect(() => init({ appId: id() })).toThrow(MissingApiURIError);
    expect(() => init({ appId: id() })).toThrow(/apiURI/);
  });

  test('normalizes the API origin and rejects non-http(s) URLs', () => {
    expect(requireApiURI('https://api.example.com/')).toBe(apiURI);
    expect(() => requireApiURI('')).toThrow(MissingApiURIError);
    expect(() => requireApiURI('ftp://api.example.com')).toThrow(/http/);
    expect(() => requireApiURI('not a url')).toThrow(/absolute/);
  });

  test('derives the websocket URL from the API origin', () => {
    expect(deriveWebsocketURI(apiURI)).toBe(
      'wss://api.example.com/runtime/session',
    );
    expect(deriveWebsocketURI('http://localhost:8888/')).toBe(
      'ws://localhost:8888/runtime/session',
    );
  });

  test('init uses the derived websocket URL unless one is given', () => {
    const derived = init({ appId: id(), apiURI: `${apiURI}/` });
    expect(derived._reactor.config.apiURI).toBe(apiURI);
    expect(derived._reactor.config.websocketURI).toBe(
      'wss://api.example.com/runtime/session',
    );

    const explicit = init({
      appId: id(),
      apiURI,
      websocketURI: 'wss://socket.example.com/runtime/session',
    });
    expect(explicit._reactor.config.websocketURI).toBe(
      'wss://socket.example.com/runtime/session',
    );
  });
});

describe('onError hook', () => {
  const newReactor = (onError?: (error: Error) => void) =>
    new Reactor({
      appId: id(),
      apiURI,
      websocketURI: 'wss://api.example.com/runtime/session',
      onError,
    });

  test('receives query errors with their trace id', () => {
    const errors: Error[] = [];
    newReactor((error) => errors.push(error)).notifyQueryError('hash', {
      message: 'Query failed',
      traceId: 'trace-1',
    });

    expect(errors).toHaveLength(1);
    expect(errors[0]).toBeInstanceOf(InstantError);
    expect(errors[0].message).toBe('Query failed');
    expect((errors[0] as InstantError).traceId).toBe('trace-1');
  });

  test('a throwing hook does not break the SDK', () => {
    const reactor = newReactor(() => {
      throw new Error('hook failed');
    });
    expect(() =>
      reactor.notifyQueryError('hash', { message: 'Query failed' }),
    ).not.toThrow();
  });
});
