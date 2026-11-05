import { Config, Effect, Option, Schema } from 'effect';
import { oauthCallbackURL } from '@instantdb/platform';
import { BadArgsError } from '../errors.ts';
import { readInstantConfigFile } from '../util/instantConfig.ts';

const HttpUrl = Schema.URL.pipe(
  Schema.filter(
    (url) =>
      url.protocol === 'http:' ||
      url.protocol === 'https:' ||
      'Expected an HTTP(S) URL',
  ),
);

const configuredBaseUrl = (management: boolean) =>
  Effect.gen(function* () {
    if (management) {
      const dashboardEnv = yield* Config.string(
        'INSTANT_CLI_DASHBOARD_API_URI',
      ).pipe(Config.option);
      if (Option.isSome(dashboardEnv)) return dashboardEnv.value;
    }
    const setEnv = yield* Config.string('INSTANT_CLI_API_URI').pipe(
      Config.option,
    );
    const dev = yield* Config.boolean('INSTANT_CLI_DEV').pipe(
      Config.withDefault(false),
    );

    if (Option.isSome(setEnv)) {
      return setEnv.value;
    }

    const instantConfig = yield* Effect.tryPromise(readInstantConfigFile);
    const configAPIURI = management
      ? (instantConfig?.dashboardApiURI ?? instantConfig?.apiURI)
      : instantConfig?.apiURI;
    if (configAPIURI !== undefined) {
      yield* Schema.decodeUnknown(HttpUrl)(configAPIURI).pipe(
        Effect.mapError(() =>
          BadArgsError.make({
            message: `Invalid ${management && instantConfig?.dashboardApiURI !== undefined ? 'dashboardApiURI' : 'apiURI'} in instant.config.ts. Expected a valid HTTP(S) URL.`,
          }),
        ),
      );
      return configAPIURI;
    }

    return dev ? 'http://localhost:8888' : 'https://api.instantdb.com';
  });

export const getBaseUrl = configuredBaseUrl(true);
export const getRuntimeBaseUrl = configuredBaseUrl(false);

export const getOAuthCallbackUrl = getRuntimeBaseUrl.pipe(
  Effect.map(oauthCallbackURL),
);

export const getDashUrl = Effect.gen(function* () {
  const setEnv = yield* Config.string('INSTANT_CLI_DASH_URI').pipe(
    Config.option,
  );
  const dev = yield* Config.boolean('INSTANT_CLI_DEV').pipe(
    Config.withDefault(false),
  );

  if (Option.isSome(setEnv)) {
    return setEnv.value;
  }

  const instantConfig = yield* Effect.tryPromise(readInstantConfigFile);
  if (instantConfig?.dashURI !== undefined) {
    yield* Schema.decodeUnknown(HttpUrl)(instantConfig.dashURI).pipe(
      Effect.mapError(() =>
        BadArgsError.make({
          message:
            'Invalid dashURI in instant.config.ts. Expected a valid HTTP(S) URL.',
        }),
      ),
    );
    return instantConfig.dashURI;
  }

  return dev ? 'http://localhost:3000' : 'https://instantdb.com';
});
