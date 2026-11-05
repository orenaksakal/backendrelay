import { readFile } from 'node:fs/promises';

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
export const DEFAULT_INTERVAL_SECONDS = 60;
export const DEFAULT_TIMEOUT_SECONDS = 10;
export const DEFAULT_METRICS_ADDR = '0.0.0.0:9465';

export class ConfigError extends Error {
  constructor(message) {
    super(message);
    this.name = 'ConfigError';
  }
}

function required(env, name) {
  const value = typeof env[name] === 'string' ? env[name].trim() : '';
  if (!value) {
    throw new ConfigError(`${name} is required`);
  }
  return value;
}

function httpUrl(value, name) {
  let url;
  try {
    url = new URL(value);
  } catch {
    throw new ConfigError(`${name} must be an absolute http(s) URL`);
  }
  if (url.protocol !== 'http:' && url.protocol !== 'https:') {
    throw new ConfigError(`${name} must be an absolute http(s) URL`);
  }
  if (url.username || url.password) {
    throw new ConfigError(`${name} must not contain credentials`);
  }
  if (url.search || url.hash) {
    throw new ConfigError(`${name} must not contain a query or fragment`);
  }
  return value.replace(/\/+$/, '');
}

function positiveInteger(value, name, min, max) {
  const text = String(value).trim();
  if (!/^\d+$/.test(text)) {
    throw new ConfigError(`${name} must be a positive integer`);
  }
  const number = Number(text);
  if (number < min || number > max) {
    throw new ConfigError(`${name} must be between ${min} and ${max}`);
  }
  return number;
}

function optionalInteger(env, name, fallback, min, max) {
  const value = typeof env[name] === 'string' ? env[name].trim() : '';
  if (!value) {
    return fallback;
  }
  return positiveInteger(value, name, min, max);
}

function metricsAddress(value) {
  let url;
  try {
    url = new URL(`http://${value}`);
  } catch {
    throw new ConfigError('CANARY_METRICS_ADDR must be host:port');
  }
  if (url.pathname !== '/' || !url.port) {
    throw new ConfigError('CANARY_METRICS_ADDR must be host:port');
  }
  return {
    host: url.hostname.replace(/^\[|\]$/g, ''),
    port: Number(url.port),
  };
}

export function parseConfig(env = process.env) {
  const apiUrl = httpUrl(required(env, 'CANARY_API_URL'), 'CANARY_API_URL');
  const appId = required(env, 'CANARY_APP_ID').toLowerCase();
  if (!UUID.test(appId)) {
    throw new ConfigError('CANARY_APP_ID must be an app UUID');
  }
  const adminTokenFile = required(env, 'CANARY_ADMIN_TOKEN_FILE');
  const healthcheck = typeof env.CANARY_HEALTHCHECK_URL === 'string'
    ? env.CANARY_HEALTHCHECK_URL.trim()
    : '';
  const healthcheckUrl = healthcheck
    ? httpUrl(healthcheck, 'CANARY_HEALTHCHECK_URL')
    : null;
  const intervalSeconds = optionalInteger(
    env,
    'CANARY_INTERVAL_SECONDS',
    DEFAULT_INTERVAL_SECONDS,
    5,
    86400,
  );
  const timeoutSeconds = optionalInteger(
    env,
    'CANARY_TIMEOUT_SECONDS',
    DEFAULT_TIMEOUT_SECONDS,
    1,
    60,
  );
  const metrics = metricsAddress(
    (typeof env.CANARY_METRICS_ADDR === 'string' && env.CANARY_METRICS_ADDR.trim())
      || DEFAULT_METRICS_ADDR,
  );
  return Object.freeze({
    apiUrl,
    appId,
    adminTokenFile,
    healthcheckUrl,
    intervalSeconds,
    intervalMs: intervalSeconds * 1000,
    timeoutSeconds,
    timeoutMs: timeoutSeconds * 1000,
    metricsHost: metrics.host,
    metricsPort: metrics.port,
  });
}

export async function readAdminToken(filePath, readFileImpl = readFile) {
  let raw;
  try {
    raw = await readFileImpl(filePath, 'utf8');
  } catch {
    throw new ConfigError('CANARY_ADMIN_TOKEN_FILE is not readable');
  }
  const token = raw.trim();
  if (!token) {
    throw new ConfigError('CANARY_ADMIN_TOKEN_FILE is empty');
  }
  if (!UUID.test(token.toLowerCase())) {
    throw new ConfigError('CANARY_ADMIN_TOKEN_FILE does not contain an admin token');
  }
  return token;
}
