import init, { compress_experimental } from './pkg/picoli_wasm.js';

let busy = false;

// Every compression gets a fresh worker, so terminating it releases the WASM heap.
self.onmessage = async ({ data }) => {
  if (busy || data?.type !== 'compress' || !(data.input instanceof ArrayBuffer)) return;
  busy = true;
  try {
    const start = performance.now();
    self.postMessage({ type: 'phase', phase: 'loading' });
    await init();
    const input = new Uint8Array(data.input);
    self.postMessage({ type: 'phase', phase: 'compressing' });
    // Omit memoryMiB to adapt automatically to each image inside the Rust core.
    const encoded = data.memoryMiB == null
      ? compress_experimental(input, data.strength)
      : compress_experimental(input, data.strength, data.memoryMiB);
    const buffer = encoded.byteOffset === 0 && encoded.byteLength === encoded.buffer.byteLength
      ? encoded.buffer : encoded.slice().buffer;
    self.postMessage({ type: 'result', buffer, milliseconds: performance.now() - start }, [buffer]);
  } catch (error) {
    self.postMessage({ type: 'error', message: error instanceof Error ? error.message : String(error) });
  }
};
