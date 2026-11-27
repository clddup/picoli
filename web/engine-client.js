/** Run the shared Rust compressor off the main thread, with real cancellation. */
export function compressFile(file, strength, { signal, memoryMiB = 128, onPhase = () => {}, onMemoryApproval = () => false, workerFactory = () => new Worker(new URL('./compress-worker.js', import.meta.url), { type: 'module' }), timeout = 900000, slowAfter = 180000 } = {}) {
  if (signal?.aborted) return Promise.reject(new DOMException('已停止压缩。', 'AbortError'));
  if (memoryMiB !== 'auto' && (!Number.isInteger(memoryMiB) || memoryMiB < 16 || memoryMiB > 1024)) return Promise.reject(new Error('内存预算必须是自动，或 16–1024 MiB 的整数。'));
  return new Promise((resolve, reject) => {
    let worker;
    let settled = false;
    let timer;
    let slowTimer;
    let compressing = false;
    let plan;
    let approved = memoryMiB !== 'auto';
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
          } else if (data?.type === 'memory-plan') {
            if (memoryMiB !== 'auto' || plan || !validPlan(data.plan)) throw new Error('压缩器返回的自动内存预算无效。');
            plan = data.plan;
            if (plan.memory_mib <= 256) {
              approved = true;
              return;
            }
            // Human approval is not compression time. Abort remains active while waiting.
            clearTimeout(timer);
            clearTimeout(slowTimer);
            onPhase('awaiting-memory');
            Promise.resolve().then(() => settled ? false : onMemoryApproval(plan)).then(allow => {
              if (settled) return;
              if (allow !== true) return abort();
              approved = true;
              armTimer();
              worker.postMessage({ type: 'approve-memory', approved: true });
            }).catch(error => finish(error));
          } else if (data?.type === 'error') {
            finish(new Error(data.message || '图片处理失败。'));
          } else if (data?.type === 'result') {
            if (!approved || (memoryMiB === 'auto' && (!plan || data.memoryMiB !== plan.memory_mib))) throw new Error('压缩器未完成内存预算确认。');
            if (!(data.buffer instanceof ArrayBuffer) || !data.buffer.byteLength) throw new Error('压缩器没有返回有效文件。');
            if (data.buffer.byteLength > file.size) throw new Error('结果大于原图，已拒绝该结果。');
            finish(null, { buffer: data.buffer, milliseconds: data.milliseconds, memoryMiB: plan?.memory_mib ?? memoryMiB });
          }
        } catch (error) {
          finish(error);
        }
      };
      file.arrayBuffer().then(input => {
        if (!settled) worker.postMessage({ type: 'compress', input, strength, memoryMiB }, [input]);
      }).catch(error => finish(error));
    } catch (error) {
      finish(error);
    }
  });
}

function validPlan(plan) {
  return plan && [128, 256, 512, 1024].includes(plan.memory_mib)
    && Number.isSafeInteger(plan.required_memory_bytes) && plan.required_memory_bytes > 0
    && plan.required_memory_bytes <= plan.memory_mib * 1024 * 1024
    && (plan.memory_mib === 128 || plan.required_memory_bytes > (plan.memory_mib / 2) * 1024 * 1024)
    && Number.isSafeInteger(plan.width) && plan.width > 0
    && Number.isSafeInteger(plan.height) && plan.height > 0
    && ['jpeg', 'png'].includes(plan.format);
}
