import test from 'node:test';
import assert from 'node:assert/strict';
import { compressFile } from '../engine-client.js';

class WorkerHarness {
  stopped = false;
  terminate() { this.stopped = true; }
  postMessage(data) { this.request = data; this.respond?.(); }
}

const file = new File(['original'], 'test.png');

test('returns actual output and terminates the worker', async () => {
  const worker = new WorkerHarness();
  const phases = [];
  worker.respond = () => {
    worker.onmessage({ data: { type: 'phase', phase: 'compressing' } });
    worker.onmessage({ data: { type: 'result', buffer: new Uint8Array([1, 2]).buffer, milliseconds: 12 } });
  };
  const result = await compressFile(file, 50, { workerFactory: () => worker, onPhase: phase => phases.push(phase) });
  assert.equal(result.buffer.byteLength, 2);
  assert.equal(worker.request.strength, 50);
  assert.equal(worker.request.memoryMiB, 128);
  assert.deepEqual(phases, ['compressing']);
  assert.equal(worker.stopped, true);
});

test('explicit budgets reach the worker without changing strength', async () => {
  for (const memoryMiB of [16, 256, 512, 1024]) {
    const worker = new WorkerHarness();
    worker.respond = () => worker.onmessage({ data: { type: 'result', buffer: new Uint8Array([1]).buffer, milliseconds: 1 } });
    await compressFile(file, 50, { memoryMiB, workerFactory: () => worker });
    assert.equal(worker.request.memoryMiB, memoryMiB);
    assert.equal(worker.request.strength, 50);
    assert.equal(worker.stopped, true);
  }
});

test('invalid budgets are rejected before creating a worker', async () => {
  for (const memoryMiB of [0, 15, 1025, 128.5, '1024', NaN]) {
    await assert.rejects(compressFile(file, 50, { memoryMiB, workerFactory() { throw new Error('must not create'); } }), /内存预算/);
  }
});

test('stop aborts a running worker; late messages cannot succeed', async () => {
  const worker = new WorkerHarness();
  const controller = new AbortController();
  const promise = compressFile(file, 50, { workerFactory: () => worker, signal: controller.signal });
  controller.abort();
  await assert.rejects(promise, { name: 'AbortError' });
  assert.equal(worker.stopped, true);
  worker.onmessage({ data: { type: 'result', buffer: new Uint8Array([1]).buffer } });
});

test('already aborted input creates no worker', async () => {
  const controller = new AbortController(); controller.abort();
  await assert.rejects(compressFile(file, 50, { signal: controller.signal, workerFactory() { throw new Error('must not create'); } }), { name: 'AbortError' });
});

test('engine rejection and missing WASM surface real errors', async () => {
  for (const kind of ['error', 'load']) {
    const worker = new WorkerHarness();
    worker.respond = () => {
      if (kind === 'error') worker.onmessage({ data: { type: 'error', message: 'unsupported input' } });
      else worker.onerror({ preventDefault() {} });
    };
    await assert.rejects(compressFile(file, 50, { workerFactory: () => worker }), kind === 'error' ? /unsupported input/ : /加载或运行失败/);
    assert.equal(worker.stopped, true);
  }
});

test('empty or larger-than-input results cannot be presented as success', async () => {
  for (const length of [0, 20]) {
    const worker = new WorkerHarness();
    worker.respond = () => worker.onmessage({ data: { type: 'result', buffer: new ArrayBuffer(length) } });
    await assert.rejects(compressFile(file, 50, { workerFactory: () => worker }), length ? /大于原图/ : /有效文件/);
    assert.equal(worker.stopped, true);
  }
});

test('hung engine times out and releases worker', async () => {
  const worker = new WorkerHarness();
  await assert.rejects(compressFile(file, 50, { workerFactory: () => worker, timeout: 5 }), /加载超时/);
  assert.equal(worker.stopped, true);
});
