// Run after building Astro with the matching PUBLIC_SELF_HOSTED and
// NEXT_PUBLIC_SELF_HOSTED values; publicEnv substitutes this flag at build time.
// Example: node scripts/astro-image-endpoint-regression.mjs --self-hosted=false
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { readFile, readdir } from 'node:fs/promises';
import { createServer, request } from 'node:http';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { resolve } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';

const mode = process.argv.find((arg) => arg.startsWith('--self-hosted='));
assert.ok(
  mode === '--self-hosted=true' || mode === '--self-hosted=false',
  'Pass --self-hosted=true or --self-hosted=false to match the Astro build.',
);
const selfHosted = mode.endsWith('=true');
const root = new URL('../', import.meta.url);
const serverDir = pathToFileURL(
  resolve(
    fileURLToPath(root),
    process.env.INSTANT_ASTRO_BUILD_DIR || 'dist-site',
    'server',
  ) + '/',
);
const fixturePath = '/relay.svg';
const fixture = await readFile(new URL(`public${fixturePath}`, root));

async function listen(server) {
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  return server.address().port;
}

async function exchange(port, path, method = 'GET') {
  return new Promise((resolve, reject) => {
    const req = request(
      { hostname: '127.0.0.1', port, path, method },
      (res) => {
        const chunks = [];
        res.on('data', (chunk) => chunks.push(chunk));
        res.on('end', () =>
          resolve({
            status: res.statusCode,
            headers: res.headers,
            body: Buffer.concat(chunks),
          }),
        );
        res.on('error', reject);
      },
    );
    req.setTimeout(5_000, () => req.destroy(new Error('Request timed out')));
    req.on('error', reject);
    req.end();
  });
}

const files = await readdir(serverDir, { recursive: true });
for (const file of files.filter((name) => name.endsWith('.mjs'))) {
  const source = await readFile(new URL(file, serverDir), 'utf8');
  assert.doesNotMatch(
    source,
    /\bimport\s*\(\s*['"]sharp['"]\s*\)/,
    `Native image decoder remains in server bundle: ${file}`,
  );
}

let remoteFetches = 0;
const remote = createServer((_request, response) => {
  remoteFetches++;
  response.writeHead(200, { 'Content-Type': 'image/avif' });
  response.end('AVIF source canary: this endpoint must never be fetched.');
});
const remotePort = await listen(remote);
const reservation = createServer();
const port = await listen(reservation);
await new Promise((resolve) => reservation.close(resolve));
const child = spawn(
  process.execPath,
  [fileURLToPath(new URL('entry.mjs', serverDir))],
  {
    cwd: fileURLToPath(root),
    env: {
      ...process.env,
      NODE_ENV: 'production',
      HOST: '127.0.0.1',
      PORT: String(port),
      NEXT_PUBLIC_SELF_HOSTED: String(selfHosted),
      PUBLIC_SELF_HOSTED: String(selfHosted),
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  },
);
let logs = '';
for (const stream of [child.stdout, child.stderr]) {
  stream.on('data', (chunk) => {
    logs = (logs + chunk).slice(-16_000);
  });
}

try {
  let ready = false;
  for (let attempt = 0; attempt < 100; attempt++) {
    assert.equal(child.exitCode, null, `Astro exited early:\n${logs}`);
    try {
      const response = await exchange(port, fixturePath);
      assert.equal(response.status, 200);
      assert.deepEqual(response.body, fixture, 'Plain image bytes changed');
      ready = true;
      break;
    } catch {
      await delay(100);
    }
  }
  assert.ok(ready, `Astro did not serve the ordinary image fixture:\n${logs}`);

  const ogImage = await exchange(port, '/opengraph-image.png');
  assert.equal(ogImage.status, 200, 'Open Graph image stopped rendering');
  assert.match(ogImage.headers['content-type'], /image\/png/);
  assert.deepEqual(
    ogImage.body.subarray(0, 8),
    Buffer.from('89504e470d0a1a0a', 'hex'),
  );

  const queryStrings = [
    '',
    `href=${encodeURIComponent(fixturePath)}&w=8&f=png`,
    `href=${encodeURIComponent(`http://127.0.0.1:${remotePort}/attack.avif`)}&f=png`,
    'href=https%3A%2F%2Fattacker.invalid%2Fattack.avif&f=avif',
    'href=data%3Aimage%2Favif%3Bbase64%2CAAAA&f=png',
    'href=%2F..%2F..%2Fetc%2Fpasswd&f=svg',
    'href=%252F..%252F..%252Fetc%252Fpasswd&f=svg',
    'href=%00&href=%2Frelay.svg&w=2147483647&h=-1',
  ];
  const paths = ['/_image', '/_image/', '/%5Fimage', '/_%69mage'];
  // TRACE/CONNECT/TRACK are rejected by the adapter's Fetch Request constructor
  // before route dispatch. Exercise standard and extension methods it accepts.
  const methods = [
    'GET',
    'HEAD',
    'POST',
    'PUT',
    'PATCH',
    'DELETE',
    'OPTIONS',
    'PURGE',
  ];
  let requests = 0;
  for (const path of paths) {
    for (const query of queryStrings) {
      for (const method of methods) {
        const target = query ? `${path}?${query}` : path;
        const response = await exchange(port, target, method);
        const label = `${method} ${target}`;
        // Astro's adapter rejects body-bearing methods before dispatching to
        // the endpoint (403); either framework rejection or our explicit 404
        // is an inert result.
        assert.ok(
          selfHosted
            ? [307, 403].includes(response.status)
            : [403, 404].includes(response.status),
          `${label}: unexpected status ${response.status}`,
        );
        if (selfHosted && response.status === 307) {
          assert.equal(response.headers.location, '/dash', label);
        } else if (response.status === 404) {
          assert.equal(
            response.headers['content-type'],
            'text/plain; charset=utf-8',
            label,
          );
          assert.equal(response.headers['cache-control'], 'no-store', label);
          assert.equal(
            response.body.toString(),
            method === 'HEAD' ? '' : 'Not found',
            label,
          );
        }
        requests++;
      }
    }
  }
  assert.equal(remoteFetches, 0, 'The disabled endpoint fetched remote input');
  console.log(
    JSON.stringify({
      selfHosted,
      requests,
      remoteFetches,
      plainImage: 'unchanged',
      ogImage: ogImage.status,
      nativeDecoderImport: 'absent',
    }),
  );
} catch (error) {
  console.error(logs);
  throw error;
} finally {
  child.kill('SIGTERM');
  const stopped = once(child, 'exit');
  const force = setTimeout(() => child.kill('SIGKILL'), 5_000);
  force.unref();
  if (child.exitCode === null && child.signalCode === null) await stopped;
  clearTimeout(force);
  await new Promise((resolve) => remote.close(resolve));
}
