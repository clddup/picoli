import test from 'node:test';
import assert from 'node:assert/strict';
import { compressFile } from '../engine-client.js';
import { readableError } from '../files.js';

const file = new File(['original'], 'test.png');

function harness({ deferResult = false } = {}) {
  let started;
  const sent = new Promise(resolve => { started = resolve; });
  const worker = {
    messages: [], stopped: false,
    terminate() { this.stopped = true; },
    result() { this.onmessage({ data: { type: 'result', buffer: new Uint8Array([1, 2]).buffer, milliseconds: 12 } }); },
    postMessage(data) {
      this.messages.push(data);
      started();
      if (!deferResult) this.result();
    },
  };
  return { worker, sent };
}

test('default and auto mode omit fixed memoryMiB so core adapts automatically without approval', async () => {
  for (const options of [{}, { memoryMiB: 'auto' }]) {
    const { worker } = harness();
    const result = await compressFile(file, 50, { ...options, workerFactory: () => worker });
    assert.equal(result.buffer.byteLength, 2);
    assert.equal('memoryMiB' in worker.messages[0], false);
    assert.equal(worker.messages[0].strength, 50);
    assert.equal(worker.messages.length, 1);
    assert.equal(worker.stopped, true);
  }
});

test('explicit budgets above 1024 MiB are not artificially blocked', async () => {
  for (const memoryMiB of [16, 256, 1024, 2048, 4096]) {
    const { worker } = harness();
    await compressFile(file, 50, { memoryMiB, workerFactory: () => worker });
    assert.equal(worker.messages[0].memoryMiB, memoryMiB);
    assert.equal(worker.stopped, true);
  }
});

test('abort during adaptive compression terminates worker and ignores late result', async () => {
  const { worker, sent } = harness({ deferResult: true });
  const controller = new AbortController();
  const result = compressFile(file, 50, { signal: controller.signal, workerFactory: () => worker });
  await sent;
  controller.abort();
  await assert.rejects(result, { name: 'AbortError' });
  worker.result();
  assert.equal(worker.stopped, true);
});

test('memory and allocation failures map to clear guidance without mentioning a budget selector', () => {
  for (const message of ['working-set estimate overflow', 'allocation failed: out of memory', 'memory budget exceeded']) {
    const readable = readableError(message);
    assert.match(readable, /内存不足.*CLI/);
    assert.doesNotMatch(readable, /调高预算/);
  }
});
