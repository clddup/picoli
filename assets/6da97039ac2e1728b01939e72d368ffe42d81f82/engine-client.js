/** Run the shared Rust compressor off the main thread, with real cancellation. */
export function compressFile(file, strength, { signal, memoryMiB, onPhase = () => {}, workerFactory = () => new Worker(new URL('./compress-worker.js', import.meta.url), { type: 'module' }), timeout = 900000, slowAfter = 180000 } = {}) {
  if (signal?.aborted) return Promise.reject(new DOMException('已停止压缩。', 'AbortError'));
  if (memoryMiB !== undefined && memoryMiB !== 'auto' && (!Number.isInteger(memoryMiB) || memoryMiB < 16)) return Promise.reject(new Error('内存预算必须是不少于 16 MiB 的整数，或留空自动适配。'));
  const requestedMemoryMiB = memoryMiB === 'auto' ? undefined : memoryMiB;
  return new Promise((resolve, reject) => {
    let worker;
    let settled = false;
    let timer;
    let slowTimer;
    let compressing = false;
    const finish = (error, value) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      clearTimeout(slowTimer);
      signal?.removeEventListener('abort', abort);
      worker?.terminate();
      if (error) reject(error); else resolve(value);
    };
    const abort = () => finish(new DOMException('已停止压缩。', 'AbortError'));
    const armTimer = () => {
      clearTimeout(timer);
      clearTimeout(slowTimer);
      // Keep a short loading watchdog; legitimate compression may take longer.
      timer = setTimeout(() => finish(new Error(compressing
        ? '这张图片处理超时，已停止。可改用 CLI，或降低强度后重试。'
        : '压缩器加载超时，已停止。请检查网络后重试。')), compressing ? timeout : Math.min(timeout, 180000));
      if (compressing && slowAfter < timeout) {
        slowTimer = setTimeout(() => {
          if (settled) return;
          try { onPhase('long-running'); } catch (error) { finish(error); }
        }, slowAfter);
      }
    };
    try {
      worker = workerFactory();
      signal?.addEventListener('abort', abort, { once: true });
      if (signal?.aborted) return abort();
      armTimer();
      worker.onerror = event => {
        event.preventDefault?.();
        finish(new Error('压缩器加载或运行失败。请刷新页面重试，或检查浏览器是否支持 WebAssembly。'));
      };
      worker.onmessageerror = () => finish(new Error('无法读取压缩器返回的数据，请重试。'));
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
            finish(new Error(data.message || '图片处理失败。'));
          } else if (data?.type === 'result') {
            if (!(data.buffer instanceof ArrayBuffer) || !data.buffer.byteLength) throw new Error('压缩器没有返回有效文件。');
            if (data.buffer.byteLength > file.size) throw new Error('结果大于原图，已拒绝该结果。');
            finish(null, { buffer: data.buffer, milliseconds: data.milliseconds });
          }
        } catch (error) {
          finish(error);
        }
      };
      file.arrayBuffer().then(input => {
        if (!settled) {
          const message = { type: 'compress', input, strength };
          if (requestedMemoryMiB !== undefined) message.memoryMiB = requestedMemoryMiB;
          worker.postMessage(message, [input]);
        }
      }).catch(error => finish(error));
    } catch (error) {
      finish(error);
    }
  });
}
