import test from 'node:test';
import assert from 'node:assert/strict';
import { compressFile } from '../engine-client.js';
import { readableError } from '../files.js';

const file = new File(['original'], 'test.png');
const small = { width: 64, height: 64, format: 'png', required_memory_bytes: 5 * 1024 * 1024, memory_mib: 128 };
const large = { width: 3000, height: 2000, format: 'png', required_memory_bytes: 300 * 1024 * 1024, memory_mib: 512 };
function harness(plan) {
  let seen;
  const planned = new Promise(resolve => { seen = resolve; });
  const worker = {
    messages: [], stopped: false,
    terminate() { this.stopped = true; },
    result() { this.onmessage({ data: { type: 'result', buffer: new Uint8Array([1, 2]).buffer, memoryMiB: plan.memory_mib, milliseconds: 12 } }); },
    postMessage(data) {
      this.messages.push(data);
      if (data.type === 'compress') {
        this.onmessage({ data: { type: 'memory-plan', plan } });
        seen();
        if (plan.memory_mib <= 256) this.result();
      } else if (data.type === 'approve-memory') this.result();
    },
  };
  return { worker, planned };
}

test('automatic small images use the minimum tier without asking for approval', async () => {
  const { worker } = harness(small);
  const result = await compressFile(file, 50, { memoryMiB: 'auto', workerFactory: () => worker, onMemoryApproval() { throw new Error('Small image must not ask'); } });
  assert.equal(result.memoryMiB, 128);
  assert.equal(worker.messages[0].memoryMiB, 'auto');
  assert.equal(worker.messages[0].strength, 50);
  assert.equal(worker.messages.length, 1);
  assert.equal(worker.stopped, true);
});

test('automatic high budgets wait for explicit approval before proceeding', async () => {
  const { worker, planned } = harness(large);
  let decide;
  const decision = new Promise(resolve => { decide = resolve; });
  const result = compressFile(file, 50, { memoryMiB: 'auto', workerFactory: () => worker, onMemoryApproval: () => decision });
  await planned;
  assert.equal(worker.messages.length, 1);
  assert.equal(worker.stopped, false);
  decide(true);
  assert.equal((await result).memoryMiB, 512);
  assert.deepEqual(worker.messages[1], { type: 'approve-memory', approved: true });
  assert.equal(worker.stopped, true);
});

test('declined or missing approval terminates the worker without compression approval', async () => {
  for (const callback of [undefined, () => false]) {
    const { worker } = harness(large);
    await assert.rejects(compressFile(file, 50, { memoryMiB: 'auto', workerFactory: () => worker, ...(callback ? { onMemoryApproval: callback } : {}) }), { name: 'AbortError' });
    assert.equal(worker.messages.length, 1);
    assert.equal(worker.stopped, true);
  }
});

test('abort during asynchronous approval ignores late consent', async () => {
  const { worker, planned } = harness(large);
  const controller = new AbortController();
  let decide;
  const decision = new Promise(resolve => { decide = resolve; });
  const result = compressFile(file, 50, { memoryMiB: 'auto', signal: controller.signal, workerFactory: () => worker, onMemoryApproval: () => decision });
  await planned;
  controller.abort();
  await assert.rejects(result, { name: 'AbortError' });
  decide(true);
  await Promise.resolve(); await Promise.resolve();
  assert.equal(worker.messages.length, 1);
  assert.equal(worker.stopped, true);
});

test('waiting for consent does not consume the compression timeout', async () => {
  const { worker, planned } = harness(large);
  let decide;
  const decision = new Promise(resolve => { decide = resolve; });
  const result = compressFile(file, 50, { memoryMiB: 'auto', timeout: 10, workerFactory: () => worker, onMemoryApproval: () => decision });
  await planned;
  await new Promise(resolve => setTimeout(resolve, 30));
  assert.equal(worker.stopped, false);
  decide(true);
  assert.equal((await result).memoryMiB, 512);
});

test('invalid, non-minimal or missing automatic plans are rejected', async () => {
  for (const plan of [{ ...small, memory_mib: 2048 }, { ...small, memory_mib: 256 }, { ...small, width: 0 }, { ...small, required_memory_bytes: 200 * 1024 * 1024 }]) {
    const { worker } = harness(plan);
    await assert.rejects(compressFile(file, 50, { memoryMiB: 'auto', workerFactory: () => worker }), /自动内存预算/);
    assert.equal(worker.stopped, true);
  }
  const worker = { terminate() {}, postMessage() { this.onmessage({ data: { type: 'result', buffer: new Uint8Array([1]).buffer, memoryMiB: 1024 } }); } };
  await assert.rejects(compressFile(file, 50, { memoryMiB: 'auto', workerFactory: () => worker }), /内存预算确认/);
});

test('automatic ceiling errors do not ask users to raise an unsupported budget', () => {
  assert.match(readableError('input exceeds resource limit: estimated memory exceeds automatic budget of 1024 MiB'), /上限.*CLI/);
});

test('approval callback errors release the worker and surface the error', async () => {
  const { worker } = harness(large);
  await assert.rejects(compressFile(file, 50, { memoryMiB: 'auto', workerFactory: () => worker, onMemoryApproval() { throw new Error('approval failed'); } }), /approval failed/);
  assert.equal(worker.stopped, true);
});
