/** Run the shared Rust compressor off the main thread, with real cancellation. */
export function compressFile(file, strength, { signal, onPhase = () => {}, workerFactory = () => new Worker(new URL('./compress-worker.js', import.meta.url), { type: 'module' }), timeout = 180000 } = {}) {
  if (signal?.aborted) return Promise.reject(new DOMException('已停止压缩。', 'AbortError'));
  return new Promise((resolve, reject) => {
    let worker;
    let settled = false;
    let timer;
    const finish = (error, value) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      signal?.removeEventListener('abort', abort);
      worker?.terminate();
      if (error) reject(error); else resolve(value);
    };
    const abort = () => finish(new DOMException('已停止压缩。', 'AbortError'));
    try {
      worker = workerFactory();
      signal?.addEventListener('abort', abort, { once: true });
      if (signal?.aborted) return abort();
      timer = setTimeout(() => finish(new Error('这张图片处理超过 3 分钟，已停止。可降低强度或尝试其他图片。')), timeout);
      worker.onerror = event => {
        event.preventDefault?.();
        finish(new Error('压缩器加载或运行失败。请刷新页面重试，或检查浏览器是否支持 WebAssembly。'));
      };
      worker.onmessageerror = () => finish(new Error('无法读取压缩器返回的数据，请重试。'));
      worker.onmessage = ({ data }) => {
        if (settled) return;
        if (data?.type === 'phase') {
          onPhase(data.phase);
        } else if (data?.type === 'error') {
          finish(new Error(data.message || '图片处理失败。'));
        } else if (data?.type === 'result') {
          if (!(data.buffer instanceof ArrayBuffer) || !data.buffer.byteLength) return finish(new Error('压缩器没有返回有效文件。'));
          if (data.buffer.byteLength > file.size) return finish(new Error('结果大于原图，已拒绝该结果。'));
          finish(null, { buffer: data.buffer, milliseconds: data.milliseconds });
        }
      };
      file.arrayBuffer().then(input => {
        if (!settled) worker.postMessage({ type: 'compress', input, strength }, [input]);
      }).catch(error => finish(error));
    } catch (error) {
      finish(error);
    }
  });
}
