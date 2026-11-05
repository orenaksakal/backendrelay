import http from 'node:http';

import { cleanupRows } from './cleanup.mjs';
import { parseConfig, readAdminToken } from './config.mjs';
import { createRecorder, renderMetrics } from './metrics.mjs';
import { classifyFailure, probeRealtime } from './probe.mjs';

function fail(event, error) {
  const reason = error instanceof Error ? error.message : 'unknown error';
  console.error(JSON.stringify({ event, reason }));
  process.exit(1);
}

const config = (() => {
  try {
    return parseConfig(process.env);
  } catch (error) {
    fail('canary_config_error', error);
  }
})();

const adminToken = await readAdminToken(config.adminTokenFile).catch((error) => {
  fail('canary_config_error', error);
});

const metrics = createRecorder();

const server = http.createServer((request, response) => {
  const path = new URL(request.url ?? '/', 'http://localhost').pathname;
  if (request.method === 'GET' && path === '/metrics') {
    response.writeHead(200, {
      'content-type': 'text/plain; version=0.0.4; charset=utf-8',
      'cache-control': 'no-store',
    });
    response.end(renderMetrics(metrics.snapshot()));
    return;
  }
  if (request.method === 'GET' && path === '/healthz') {
    response.writeHead(200, { 'content-type': 'text/plain; charset=utf-8' });
    response.end('ok\n');
    return;
  }
  response.writeHead(404, { 'content-type': 'text/plain; charset=utf-8' });
  response.end('not found\n');
});

async function pingHealthcheck() {
  if (!config.healthcheckUrl) {
    return;
  }
  try {
    const response = await fetch(config.healthcheckUrl, {
      signal: AbortSignal.timeout(config.timeoutMs),
    });
    try {
      await response.body?.cancel?.();
    } catch {
      // The ping body is irrelevant.
    }
    if (!response.ok) {
      console.warn(
        JSON.stringify({
          event: 'canary_healthcheck_failed',
          status: response.status,
        }),
      );
    }
  } catch {
    console.warn(JSON.stringify({ event: 'canary_healthcheck_failed' }));
  }
}

async function runOnce() {
  const result = await probeRealtime({
    apiUrl: config.apiUrl,
    appId: config.appId,
    adminToken,
    timeoutMs: config.timeoutMs,
  });
  const at = Date.now();
  if (result.outcome === 'success') {
    metrics.recordSuccess(result.propagationSeconds, at);
    console.log(
      JSON.stringify({
        event: 'canary_probe',
        outcome: 'success',
        propagation_ms: Math.round(result.propagationSeconds * 1000),
      }),
    );
    await pingHealthcheck();
    try {
      const pruned = await cleanupRows({
        apiUrl: config.apiUrl,
        appId: config.appId,
        adminToken,
        timeoutMs: config.timeoutMs,
      });
      if (pruned > 0) {
        console.log(JSON.stringify({ event: 'canary_pruned', rows: pruned }));
      }
    } catch (error) {
      console.warn(
        JSON.stringify({
          event: 'canary_cleanup_failed',
          reason: error instanceof Error ? error.message : 'unknown error',
        }),
      );
    }
    return;
  }
  metrics.recordFailure(result.outcome);
  console.warn(
    JSON.stringify({ event: 'canary_probe', outcome: result.outcome }),
  );
}

let running = true;

async function loop() {
  let nextRunAt = Date.now();
  while (running) {
    try {
      await runOnce();
    } catch (error) {
      const outcome = classifyFailure(error);
      metrics.recordFailure(outcome);
      console.warn(
        JSON.stringify({
          event: 'canary_probe',
          outcome,
          reason: error instanceof Error ? error.message : 'unknown error',
        }),
      );
    }
    nextRunAt += config.intervalMs;
    if (nextRunAt < Date.now()) {
      nextRunAt = Date.now();
    }
    await new Promise((resolve) => {
      const timer = setTimeout(resolve, Math.max(0, nextRunAt - Date.now()));
      timer.unref?.();
    });
  }
}

for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, () => {
    running = false;
    server.close(() => process.exit(0));
    setTimeout(() => process.exit(0), 5000).unref();
  });
}

server.on('error', (error) => fail('canary_listen_error', error));
server.listen(config.metricsPort, config.metricsHost, () => {
  console.log(
    JSON.stringify({
      event: 'canary_started',
      metrics_host: config.metricsHost,
      metrics_port: config.metricsPort,
      interval_seconds: config.intervalSeconds,
    }),
  );
  void loop();
});
