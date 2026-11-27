import test from 'node:test';
import assert from 'node:assert/strict';
import { compressBatch, defaultConcurrency, taskWeight } from '../engine-client.js';

function makeEntry(name, { bytes = 100, dimensions } = {}) {
  return {
    id: name,
    file: new File([new Uint8Array(bytes)], name),
    dimensions,
  };
}

test('defaultConcurrency scales cleanly with hardware cores', () => {
  assert.equal(defaultConcurrency(1), 1);
  assert.equal(defaultConcurrency(2), 1);
  assert.equal(defaultConcurrency(4), 3);
  assert.equal(defaultConcurrency(8), 6);
  assert.equal(defaultConcurrency(16), 6);
  assert.equal(defaultConcurrency(null), 2);
  assert.equal(defaultConcurrency(NaN), 2);
});

test('taskWeight throttles large images to protect browser memory', () => {
  assert.equal(taskWeight(makeEntry('small.png', { bytes: 200_000, dimensions: { width: 800, height: 600 } }), 4), 1);
  assert.equal(taskWeight(makeEntry('medium.png', { bytes: 500_000, dimensions: { width: 2000, height: 2000 } }), 4), 2);
  assert.equal(taskWeight(makeEntry('huge.png', { bytes: 6_000_000, dimensions: { width: 4464, height: 3173 } }), 4), 4);
});

test('compressBatch runs multiple small images concurrently, reuses workers, and terminates all workers on finish', async () => {
  const workers = [];
  let inFlight = 0;
  let peakInFlight = 0;
  const workerFactory = () => {
    const worker = {
      runs: 0,
      stopped: false,
      terminate() { this.stopped = true; },
      postMessage(data) {
        this.runs += 1;
        inFlight += 1;
        peakInFlight = Math.max(peakInFlight, inFlight);
        this.onmessage?.({ data: { type: 'phase', phase: 'compressing' } });
        setTimeout(() => {
          inFlight -= 1;
          this.onmessage?.({ data: { type: 'result', buffer: new Uint8Array([1, 2, 3]).buffer, milliseconds: 15 } });
        }, 10);
      },
    };
    workers.push(worker);
    return worker;
  };

  const entries = Array.from({ length: 6 }, (_, i) => makeEntry(`img-${i}.jpg`, { bytes: 50, dimensions: { width: 500, height: 500 } }));
  const completed = [];
  await compressBatch(entries, 50, {
    concurrency: 3,
    workerFactory,
    onResult: (entry, res) => completed.push([entry.id, res.buffer.byteLength]),
  });

  assert.equal(completed.length, 6);
  assert.equal(peakInFlight, 3);
  assert.equal(workers.length, 3, 'workers should be reused across the 6 small images');
  assert.ok(workers.every(w => w.stopped), 'all workers must be terminated when batch finishes');
});

test('compressBatch isolates huge images and recycles their worker immediately', async () => {
  const workers = [];
  let inFlight = 0;
  let peakDuringHuge = 0;
  const workerFactory = () => {
    const worker = {
      stopped: false,
      terminate() { this.stopped = true; },
      postMessage(data) {
        inFlight += 1;
        if (data.input.byteLength > 1000) peakDuringHuge = Math.max(peakDuringHuge, inFlight);
        setTimeout(() => {
          inFlight -= 1;
          this.onmessage?.({ data: { type: 'result', buffer: new Uint8Array([1]).buffer, milliseconds: 10 } });
        }, 10);
      },
    };
    workers.push(worker);
    return worker;
  };

  const huge = makeEntry('huge.png', { bytes: 2000, dimensions: { width: 4464, height: 3173 } });
  const small = makeEntry('small.png', { bytes: 50, dimensions: { width: 400, height: 400 } });
  const done = [];
  await compressBatch([huge, small], 50, {
    concurrency: 4,
    workerFactory,
    onResult: entry => done.push(entry.id),
  });

  assert.deepEqual(done, ['huge.png', 'small.png']);
  assert.equal(peakDuringHuge, 1);
  assert.equal(workers.length, 2, 'huge image worker must be terminated instead of reused');
  assert.ok(workers.every(w => w.stopped));
});

test('compressBatch abort terminates all live workers and skips unstarted entries', async () => {
  const workers = [];
  const controller = new AbortController();
  let startedCount = 0;
  const workerFactory = () => {
    const worker = {
      stopped: false,
      terminate() { this.stopped = true; },
      postMessage() {
        startedCount += 1;
        if (startedCount === 2) controller.abort();
      },
    };
    workers.push(worker);
    return worker;
  };

  const entries = Array.from({ length: 5 }, (_, i) => makeEntry(`img-${i}.png`, { bytes: 50, dimensions: { width: 400, height: 400 } }));
  const errors = [];
  await compressBatch(entries, 50, {
    concurrency: 2,
    signal: controller.signal,
    workerFactory,
    onError: (entry, err) => errors.push([entry.id, err.name]),
  });

  assert.equal(workers.length, 2);
  assert.ok(workers.every(w => w.stopped));
  assert.equal(errors.length, 2);
  assert.ok(errors.every(([, name]) => name === 'AbortError'));
});
