import init, { compress_experimental } from './pkg/picoli_wasm.js';

let initialized = false;
let initPromise = null;
let busy = false;

async function ensureInit() {
  if (initialized) return;
  if (!initPromise) {
    initPromise = init().then(() => { initialized = true; }).catch(error => {
      initPromise = null;
      throw error;
    });
  }
  await initPromise;
}

// Reuses the compiled WASM instance across small/medium files; callers
// terminate the worker after large images or batch completion to reclaim heap.
self.onmessage = async ({ data }) => {
  if (busy || data?.type !== 'compress' || !(data.input instanceof ArrayBuffer)) return;
  busy = true;
  try {
    const start = performance.now();
    if (!initialized) {
      self.postMessage({ type: 'phase', phase: 'loading' });
      await ensureInit();
    }
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
  } finally {
    busy = false;
  }
};
