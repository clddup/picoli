/** Run the shared Rust compressor off the main thread, with real cancellation. */
const DEFAULT_WORKER_FACTORY = () => new Worker(new URL('./compress-worker.js', import.meta.url), { type: 'module' });
const RECYCLE_PIXELS = 4_000_000;
const RECYCLE_BYTES = 4 * 1024 * 1024;

function normalizeMemoryOption(memoryMiB) {
  if (memoryMiB !== undefined && memoryMiB !== 'auto' && (!Number.isInteger(memoryMiB) || memoryMiB < 16)) {
    throw new Error('内存预算必须是不少于 16 MiB 的整数，或留空自动适配。');
  }
  return memoryMiB === 'auto' ? undefined : memoryMiB;
}

function runOnWorker(worker, file, strength, requestedMemoryMiB, { signal, onPhase = () => {}, timeout = 900000, slowAfter = 180000, terminateOnFinish = true } = {}) {
  return new Promise((resolve, reject) => {
    let settled = false;
    let timer;
    let slowTimer;
    let compressing = false;
    const clearHandlers = () => {
      const noop = () => {};
      worker.onmessage = noop;
      worker.onerror = noop;
      worker.onmessageerror = noop;
    };
    const finish = (error, value, forceTerminate = false) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      clearTimeout(slowTimer);
      signal?.removeEventListener('abort', abort);
      clearHandlers();
      if (terminateOnFinish || forceTerminate || error) worker.terminate();
      if (error) reject(error); else resolve(value);
    };
    const abort = () => finish(new DOMException('已停止压缩。', 'AbortError'), null, true);
    const armTimer = () => {
      clearTimeout(timer);
      clearTimeout(slowTimer);
      // Keep a short loading watchdog; legitimate compression may take longer.
      timer = setTimeout(() => finish(new Error(compressing
        ? '这张图片处理超时，已停止。可改用 CLI，或降低强度后重试。'
        : '压缩器加载超时，已停止。请检查网络后重试。'), null, true), compressing ? timeout : Math.min(timeout, 180000));
      if (compressing && slowAfter < timeout) {
        slowTimer = setTimeout(() => {
          if (settled) return;
          try { onPhase('long-running'); } catch (error) { finish(error, null, true); }
        }, slowAfter);
      }
    };
    try {
      signal?.addEventListener('abort', abort, { once: true });
      if (signal?.aborted) return abort();
      armTimer();
      worker.onerror = event => {
        event.preventDefault?.();
        finish(new Error('压缩器加载或运行失败。请刷新页面重试，或检查浏览器是否支持 WebAssembly。'), null, true);
      };
      worker.onmessageerror = () => finish(new Error('无法读取压缩器返回的数据，请重试。'), null, true);
      worker.onmessage = ({ data }) => {
        if (settled) return;
        try {
          if (data?.type === 'phase') {
            if (data.phase === 'compressing' && !compressing) {
              compressing = true;
              armTimer();
            }
            onPhase(data.phase);
          } else if (data?.type === 'error') {
            finish(new Error(data.message || '图片处理失败。'), null, true);
          } else if (data?.type === 'result') {
            if (!(data.buffer instanceof ArrayBuffer) || !data.buffer.byteLength) throw new Error('压缩器没有返回有效文件。');
            if (data.buffer.byteLength > file.size) throw new Error('结果大于原图，已拒绝该结果。');
            finish(null, { buffer: data.buffer, milliseconds: data.milliseconds });
          }
        } catch (error) {
          finish(error, null, true);
        }
      };
      file.arrayBuffer().then(input => {
        if (!settled) {
          const message = { type: 'compress', input, strength };
          if (requestedMemoryMiB !== undefined) message.memoryMiB = requestedMemoryMiB;
          worker.postMessage(message, [input]);
        }
      }).catch(error => finish(error, null, true));
    } catch (error) {
      finish(error, null, true);
    }
  });
}

export function compressFile(file, strength, { signal, memoryMiB, onPhase = () => {}, workerFactory = DEFAULT_WORKER_FACTORY, timeout = 900000, slowAfter = 180000 } = {}) {
  if (signal?.aborted) return Promise.reject(new DOMException('已停止压缩。', 'AbortError'));
  let requestedMemoryMiB;
  try {
    requestedMemoryMiB = normalizeMemoryOption(memoryMiB);
  } catch (error) {
    return Promise.reject(error);
  }
  let worker;
  try {
    worker = workerFactory();
  } catch (error) {
    return Promise.reject(error);
  }
  return runOnWorker(worker, file, strength, requestedMemoryMiB, {
    signal,
    onPhase,
    timeout,
    slowAfter,
    terminateOnFinish: true,
  });
}

/** Choose default batch concurrency from hardware cores, leaving headroom for the UI thread. */
export function defaultConcurrency(hardwareConcurrency = globalThis.navigator?.hardwareConcurrency) {
  if (!Number.isFinite(hardwareConcurrency) || hardwareConcurrency < 1) return 2;
  const cores = Math.floor(hardwareConcurrency);
  if (cores <= 2) return 1;
  if (cores <= 4) return cores - 1;
  return Math.min(6, cores - 1);
}

/** Weight each image so huge images run with reduced concurrency to avoid browser OOM. */
export function taskWeight(entry, maxSlots) {
  const pixels = entry.dimensions ? entry.dimensions.width * entry.dimensions.height : 0;
  const bytes = entry.file?.size ?? 0;
  if (pixels >= 8_000_000 || bytes >= 8 * 1024 * 1024) return maxSlots;
  if (pixels >= 3_000_000 || bytes >= 3 * 1024 * 1024) return Math.min(maxSlots, 2);
  return 1;
}

function shouldRecycleWorker(entry) {
  const pixels = entry.dimensions ? entry.dimensions.width * entry.dimensions.height : 0;
  const bytes = entry.file?.size ?? 0;
  return pixels >= RECYCLE_PIXELS || bytes >= RECYCLE_BYTES;
}

/**
 * Compress a batch of entries concurrently across a bounded Web Worker pool.
 * Reuses workers for small/medium images and terminates workers after large images
 * or once the batch settles/aborts so WASM linear memory is always reclaimed.
 */
export async function compressBatch(entries, strength, {
  signal,
  memoryMiB,
  concurrency = defaultConcurrency(),
  workerFactory = DEFAULT_WORKER_FACTORY,
  timeout = 900000,
  slowAfter = 180000,
  onStart = () => {},
  onPhase = () => {},
  onResult = () => {},
  onError = () => {},
} = {}) {
  if (signal?.aborted) throw new DOMException('已停止压缩。', 'AbortError');
  const requestedMemoryMiB = normalizeMemoryOption(memoryMiB);
  const maxSlots = Math.max(1, Math.min(8, Math.floor(concurrency) || 1));
  const queue = entries.slice();
  const idleWorkers = [];
  const liveWorkers = new Set();
  let usedSlots = 0;
  let activeCount = 0;

  const terminateAll = () => {
    for (const worker of liveWorkers) {
      try { worker.terminate(); } catch {}
    }
    liveWorkers.clear();
    idleWorkers.length = 0;
  };

  if (signal) signal.addEventListener('abort', terminateAll, { once: true });

  const acquireWorker = () => {
    if (idleWorkers.length) return idleWorkers.pop();
    const worker = workerFactory();
    liveWorkers.add(worker);
    return worker;
  };

  const releaseWorker = (worker, recycle) => {
    if (recycle || signal?.aborted) {
      liveWorkers.delete(worker);
      try { worker.terminate(); } catch {}
    } else if (liveWorkers.has(worker)) {
      idleWorkers.push(worker);
    }
  };

  try {
    await new Promise(resolve => {
      const pump = () => {
        if (signal?.aborted || (!queue.length && activeCount === 0)) {
          if (activeCount === 0) resolve();
          return;
        }
        while (!signal?.aborted && queue.length) {
          const next = queue[0];
          const weight = taskWeight(next, maxSlots);
          if (activeCount > 0 && usedSlots + weight > maxSlots) break;
          queue.shift();
          usedSlots += weight;
          activeCount += 1;

          let worker;
          let recycled = false;
          try {
            onStart(next);
            worker = acquireWorker();
          } catch (error) {
            usedSlots -= weight;
            activeCount -= 1;
            onError(next, error);
            if (!queue.length && activeCount === 0) { resolve(); return; }
            continue;
          }

          const recycleAfter = shouldRecycleWorker(next);
          runOnWorker(worker, next.file, strength, requestedMemoryMiB, {
            signal,
            timeout,
            slowAfter,
            terminateOnFinish: recycleAfter,
            onPhase: phase => onPhase(next, phase),
          }).then(result => {
            if (recycleAfter) {
              liveWorkers.delete(worker);
              recycled = true;
            }
            onResult(next, result);
          }).catch(error => {
            liveWorkers.delete(worker);
            recycled = true;
            onError(next, error);
          }).finally(() => {
            if (!recycled) releaseWorker(worker, false);
            usedSlots -= weight;
            activeCount -= 1;
            pump();
          });
        }
      };
      pump();
    });
  } finally {
    signal?.removeEventListener('abort', terminateAll);
    terminateAll();
  }
}
