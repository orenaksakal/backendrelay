export class MissingApiURIError extends Error {
  constructor(sdk: string) {
    super(
      `${sdk} requires \`apiURI\`. Pass the BackendRelay API origin, for example ` +
        `init({ appId, apiURI: 'https://api.example.com' }). The SDK no longer ` +
        `falls back to instantdb.com.`,
    );
    this.name = 'MissingApiURIError';
  }
}

/**
 * Validates the configured API origin and strips trailing slashes so request
 * paths are built consistently.
 */
export function requireApiURI(
  apiURI: string | null | undefined,
  sdk = 'Instant',
): string {
  const trimmed = apiURI?.trim();
  if (!trimmed) {
    throw new MissingApiURIError(sdk);
  }

  let url: URL;
  try {
    url = new URL(trimmed);
  } catch (_e) {
    throw new Error(
      `${sdk} \`apiURI\` must be an absolute http(s) URL. Received: ${trimmed}`,
    );
  }
  if (url.protocol !== 'http:' && url.protocol !== 'https:') {
    throw new Error(
      `${sdk} \`apiURI\` must use http:// or https://. Received: ${trimmed}`,
    );
  }

  return trimmed.replace(/\/+$/, '');
}

/**
 * Derives the realtime session URL from the API origin:
 * `https://api.example.com` becomes `wss://api.example.com/runtime/session`.
 */
export function deriveWebsocketURI(apiURI: string): string {
  const url = new URL(requireApiURI(apiURI));
  url.protocol = url.protocol === 'http:' ? 'ws:' : 'wss:';
  return `${url.toString().replace(/\/+$/, '')}/runtime/session`;
}
