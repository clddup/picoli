import init, { compress_experimental } from './pkg/picoli_wasm.js';

// Every compression gets a fresh worker, so terminating it really releases the WASM heap.
self.onmessage = async ({ data }) => {
  if (data?.type !== 'compress' || !(data.input instanceof ArrayBuffer)) return;
  try {
    const start = performance.now();
    self.postMessage({ type: 'phase', phase: 'loading' });
    await init();
    self.postMessage({ type: 'phase', phase: 'compressing' });
    // Budget is chosen explicitly in the UI; omission keeps the core's default 128 MiB.
    const encoded = compress_experimental(new Uint8Array(data.input), data.strength, data.memoryMiB);
    const buffer = encoded.byteOffset === 0 && encoded.byteLength === encoded.buffer.byteLength
      ? encoded.buffer : encoded.slice().buffer;
    self.postMessage({ type: 'result', buffer, milliseconds: performance.now() - start }, [buffer]);
  } catch (error) {
    self.postMessage({ type: 'error', message: error instanceof Error ? error.message : String(error) });
  }
};
