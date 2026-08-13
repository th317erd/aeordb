#!/usr/bin/env node

'use strict';

import fs from 'node:fs/promises';

const BLOB_PAYLOAD = Buffer.from('p4-8e-qualification-blob-payload-v1\n', 'utf8');
const BLOB_HASH    = '5b5cb25c9365b67b6648d6a273e0b7d0a80719903fd8af7355a89f6bea335f5c';

function parseArguments(argumentsList) {
  let options = {
    baseURL:      'http://127.0.0.1:16980',
    durationSecs: 30,
    report:       null,
  };

  for (let index = 2; index < argumentsList.length; index++) {
    let argument = argumentsList[index];
    let value    = argumentsList[index + 1];
    if (argument === '--base-url' && value) {
      options.baseURL = value.replace(/\/$/, '');
      index++;
    } else if (argument === '--duration-secs' && value) {
      options.durationSecs = Number.parseInt(value, 10);
      index++;
    } else if (argument === '--report' && value) {
      options.report = value;
      index++;
    } else {
      throw new Error(`Unknown or incomplete argument: ${argument}`);
    }
  }

  if (!Number.isSafeInteger(options.durationSecs) || options.durationSecs < 5)
    throw new Error('--duration-secs must be an integer of at least 5');

  if (!options.report)
    throw new Error('--report is required');

  return options;
}

function sleep(milliseconds) {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

function percentile(values, ratio) {
  if (values.length === 0)
    return null;

  let sorted = values.toSorted((left, right) => left - right);
  let index  = Math.min(sorted.length - 1, Math.ceil(sorted.length * ratio) - 1);
  return sorted[index];
}

function maximum(values) {
  return values.reduce((largest, value) => Math.max(largest, value), Number.NEGATIVE_INFINITY);
}

async function main() {
  let options      = parseArguments(process.argv);
  let deadline     = Date.now() + (options.durationSecs * 1_000);
  let failures     = [];
  let healthMillis = [];
  let counters     = {
    blobCommits:            0,
    blobUploads:            0,
    fileReads:              0,
    fileWrites:             0,
    gcCancellationAttempts: 0,
    gcRuns:                 0,
    healthSamples:          0,
    reindexTasks:           0,
    searches:               0,
  };

  async function request(path, init = {}, expectedStatuses = [ 200 ]) {
    let controller = new AbortController();
    let timeoutID  = setTimeout(() => controller.abort(), 30_000);
    let startedAt  = performance.now();
    try {
      let response = await fetch(`${options.baseURL}${path}`, {
        ...init,
        signal:  controller.signal,
        headers: {
          ...(init.body) ? { 'content-type': 'application/json' } : {},
          ...(init.headers || {}),
        },
      });
      let elapsedMilliseconds = performance.now() - startedAt;
      if (!expectedStatuses.includes(response.status)) {
        let body = await response.text();
        throw new Error(`${init.method || 'GET'} ${path} returned ${response.status}: ${body.slice(0, 500)}`);
      }
      return { response, elapsedMilliseconds };
    } finally {
      clearTimeout(timeoutID);
    }
  }

  async function record(operation, callback) {
    try {
      await callback();
    } catch (error) {
      failures.push(`${operation}: ${error.stack || error}`);
    }
  }

  await record('seed files', async () => {
    for (let index = 0; index < 64; index++) {
      let payload = JSON.stringify({ index, phase: 'seed', text: `qualification document ${index}` });
      await request(`/files/qualify/seed-${index}.json`, {
        method:  'PUT',
        headers: { 'content-type': 'application/json' },
        body:    payload,
      }, [ 200, 201 ]);
    }
  });

  await record('blob check', async () => {
    await request('/blobs/check', {
      method: 'POST',
      body:   JSON.stringify({ hashes: [ BLOB_HASH ] }),
    });
  });

  await record('blob upload', async () => {
    await request(`/blobs/chunks/${BLOB_HASH}`, {
      method:  'PUT',
      headers: { 'content-type': 'application/octet-stream' },
      body:    BLOB_PAYLOAD,
    }, [ 200, 201 ]);
    counters.blobUploads++;
  });

  async function writer() {
    let sequence = 0;
    while (Date.now() < deadline) {
      await record('file write', async () => {
        let path    = `/files/qualify/writes/write-${sequence % 256}.json`;
        let payload = JSON.stringify({ sequence, writtenAt: Date.now(), text: `qualification write ${sequence}` });
        await request(path, {
          method:  'PUT',
          headers: { 'content-type': 'application/json' },
          body:    payload,
        }, [ 200, 201 ]);
        counters.fileWrites++;
      });
      sequence++;
      await sleep(10);
    }
  }

  async function reader() {
    let sequence = 0;
    while (Date.now() < deadline) {
      await record('file read', async () => {
        let { response } = await request(`/files/qualify/seed-${sequence % 64}.json`);
        await response.arrayBuffer();
        counters.fileReads++;
      });
      if (sequence % 8 === 0) {
        await record('directory listing', async () => {
          let { response } = await request('/files/qualify/');
          await response.arrayBuffer();
        });
      }
      sequence++;
      await sleep(10);
    }
  }

  async function searcher() {
    let sequence = 0;
    while (Date.now() < deadline) {
      await record('file search', async () => {
        let payload = {
          path:  '/qualify/',
          where: { field: '@filename', op: 'eq', value: `seed-${sequence % 64}.json` },
          limit: 10,
        };
        let { response } = await request('/files/search', { method: 'POST', body: JSON.stringify(payload) });
        await response.arrayBuffer();
        counters.searches++;
      });
      sequence++;
      await sleep(25);
    }
  }

  async function blobCommitter() {
    let sequence = 0;
    while (Date.now() < deadline) {
      await record('blob commit', async () => {
        let payload = {
          files: [ {
            path:         `/qualify/blobs/blob-${sequence % 128}.txt`,
            chunks:       [ BLOB_HASH ],
            content_type: 'text/plain',
            size:         BLOB_PAYLOAD.length,
          } ],
        };
        await request('/blobs/commit', { method: 'POST', body: JSON.stringify(payload) });
        counters.blobCommits++;
      });
      sequence++;
      await sleep(40);
    }
  }

  async function healthSampler() {
    while (Date.now() < deadline) {
      await record('health', async () => {
        let { response, elapsedMilliseconds } = await request('/system/health');
        await response.arrayBuffer();
        healthMillis.push(elapsedMilliseconds);
        counters.healthSamples++;
      });
      await sleep(20);
    }
  }

  async function maintainer() {
    let sequence = 0;
    while (Date.now() < deadline) {
      if (sequence % 2 === 0) {
        await record('dry-run GC', async () => {
          await request('/system/gc?dry_run=true', { method: 'POST' });
          counters.gcRuns++;
        });
      } else {
        await record('metadata reindex', async () => {
          let payload = {
            path:               '/qualify/',
            force:              false,
            metadata_only:      true,
            index_flush_writes: 64,
            index_flush_ms:     100,
          };
          await request('/system/tasks/reindex', { method: 'POST', body: JSON.stringify(payload) });
          counters.reindexTasks++;
        });
      }
      sequence++;
      await sleep(1_000);
    }
  }

  async function cancellationProbe() {
    await sleep(250);
    while (Date.now() < deadline) {
      let controller = new AbortController();
      let requestPromise = fetch(`${options.baseURL}/system/gc?dry_run=true`, {
        method: 'POST',
        signal: controller.signal,
      });
      controller.abort();
      try {
        await requestPromise;
      } catch (error) {
        if (error.name !== 'AbortError')
          failures.push(`GC cancellation: ${error.stack || error}`);
      }
      counters.gcCancellationAttempts++;
      await sleep(2_000);
    }
  }

  await Promise.all([ writer(), reader(), searcher(), blobCommitter(), healthSampler(), maintainer(), cancellationProbe() ]);

  await record('terminal dry-run GC', async () => {
    await request('/system/gc?dry_run=true', { method: 'POST' });
    counters.gcRuns++;
  });
  await record('terminal health', async () => {
    let { response, elapsedMilliseconds } = await request('/system/health');
    await response.arrayBuffer();
    healthMillis.push(elapsedMilliseconds);
    counters.healthSamples++;
  });

  let report = {
    schema: 'aeordb-v4-p4-8e-load-v1',
    durationSeconds: options.durationSecs,
    counters,
    failures,
    health: {
      maximumMilliseconds: (healthMillis.length > 0) ? maximum(healthMillis) : null,
      p50Milliseconds: percentile(healthMillis, 0.50),
      p95Milliseconds: percentile(healthMillis, 0.95),
      p99Milliseconds: percentile(healthMillis, 0.99),
    },
  };
  await fs.writeFile(options.report, `${JSON.stringify(report, null, 2)}\n`, 'utf8');

  if (failures.length > 0)
    throw new Error(`qualification workload recorded ${failures.length} failures; see ${options.report}`);

  if (report.health.maximumMilliseconds > 5_000 || report.health.p99Milliseconds > 1_000)
    throw new Error(`health latency exceeded contract: ${JSON.stringify(report.health)}`);
}

main().catch((error) => {
  console.error(error.stack || error);
  process.exitCode = 1;
});
