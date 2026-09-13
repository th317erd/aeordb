'use strict';

// Target-behavior gate: run against a normal release binary, never a migration
// helper. This deliberately fails until ordinary new databases really use v4.
// Example: AEORDB_TEST_BINARY=/absolute/aeordb AEORDB_TEST_ROOT=/private/scratch
//          timeout 240s node --test scripts/spec/v4-default-live-spec.mjs
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { closeSync, openSync } from 'node:fs';
import { mkdtemp, open, readFile, writeFile } from 'node:fs/promises';
import { createServer } from 'node:net';
import { isAbsolute, join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import test from 'node:test';

test('normal CLI creates v4 and preserves real HTTP writes across restart', { timeout: 210_000 }, async (context) => {
  const binary = process.env.AEORDB_TEST_BINARY;
  const root = process.env.AEORDB_TEST_ROOT;
  assert.ok(binary && isAbsolute(binary), 'AEORDB_TEST_BINARY must be an absolute normal-release binary path');
  assert.ok(root && isAbsolute(root), 'AEORDB_TEST_ROOT must be an existing private disposable-test directory');
  const directory = await mkdtemp(join(root, 'v4-default-'));
  const database = join(directory, 'new.aeordb');
  const binarySHA256 = createHash('sha256').update(await readFile(binary)).digest('hex');
  context.diagnostic(`evidence=${directory}; binary_sha256=${binarySHA256}`);
  const listener = createServer();
  await new Promise((resolve, reject) => {
    listener.once('error', reject);
    listener.listen(0, '127.0.0.1', resolve);
  });
  const port = listener.address().port;
  await new Promise((resolve, reject) => listener.close((error) => (error ? reject(error) : resolve())));
  const baseURL = `http://127.0.0.1:${port}`;
  let server;
  let serverExit;
  let serverExited = false;
  const statuses = [];

  async function request(path, expectedStatus, options = {}) {
    const response = await fetch(`${baseURL}${path}`, { ...options, signal: AbortSignal.timeout(5_000) });
    statuses.push({ method: options.method || 'GET', path, expectedStatus, actualStatus: response.status });
    assert.equal(response.status, expectedStatus, `${options.method || 'GET'} ${path}`);
    return Buffer.from(await response.arrayBuffer());
  }

  async function start(label) {
    const log = openSync(join(directory, `${label}.log`), 'wx', 0o600);
    try {
      serverExited = false;
      server = spawn(binary, ['start', '-D', database, '--host', '127.0.0.1', '--port', String(port), '--auth', 'disabled', '--log-format', 'json'], {
        env: { ...process.env, TMPDIR: directory },
        stdio: ['ignore', log, log],
      });
      serverExit = new Promise((resolve) => {
        server.once('error', (error) => { serverExited = true; resolve({ error }); });
        server.once('exit', (code, signal) => { serverExited = true; resolve({ code, signal }); });
      });
    } finally {
      closeSync(log);
    }
    const deadline = Date.now() + 60_000;
    while (Date.now() < deadline) {
      assert.equal(serverExited, false, `server exited before health; inspect ${label}.log`);
      try {
        const response = await fetch(`${baseURL}/system/health`, { signal: AbortSignal.timeout(1_000) });
        if (response.ok && (await response.json()).status === 'healthy')
          return;
      } catch {
        // Startup connection refusal is bounded by the deadline and child exit.
      }
      await delay(250);
    }
    assert.fail(`server did not become healthy; inspect ${label}.log`);
  }

  async function stop() {
    if (!server)
      return;
    if (!serverExited)
      server.kill('SIGTERM');
    const cancellation = new AbortController();
    try {
      const result = await Promise.race([
        serverExit,
        delay(60_000, null, { signal: cancellation.signal }).then(() => { throw new Error('graceful shutdown timed out; preserve test process and evidence'); }),
      ]);
      server = undefined;
      assert.deepEqual(result, { code: 0, signal: null }, 'normal shutdown must succeed');
    } finally {
      cancellation.abort();
    }
  }

  const payload = Buffer.from([0, 1, 2, 127, 128, 254, 255, 65, 69, 79, 82]);
  try {
    await start('create');
    await request('/files/v4-default/payload.bin', 201, {
      method: 'PUT', headers: { 'Content-Type': 'application/octet-stream' }, body: payload,
    });
    assert.deepEqual(await request('/files/v4-default/payload.bin', 200), payload);
    await stop();
    await start('reopen');
    assert.deepEqual(await request('/files/v4-default/payload.bin', 200), payload);
    await request('/files/v4-default/payload.bin', 200, { method: 'DELETE' });
    await request('/files/v4-default/payload.bin', 404);
    await request('/files/v4-default/never-existed.bin', 404);
    await stop();

    // Independent, bounded byte probe. This proves the default-format choice,
    // not complete v4 header validity (covered by the separate format oracle).
    const handle = await open(database, 'r');
    const prefix = Buffer.alloc(5);
    try {
      assert.equal((await handle.read(prefix, 0, prefix.length, 0)).bytesRead, 5);
    } finally {
      await handle.close();
    }
    await writeFile(join(directory, 'observation.json'), `${JSON.stringify({ binary, binarySHA256, database, prefixHex: prefix.toString('hex'), observedVersion: prefix[4], expectedVersion: 4, statuses }, null, 2)}\n`, { flag: 'wx', mode: 0o600 });
    assert.equal(prefix.subarray(0, 4).toString('ascii'), 'AEOR');
    assert.equal(prefix[4], 4, 'ordinary CLI creation must emit v4, not a legacy-format database');
  } finally {
    await stop();
  }
});
