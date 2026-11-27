import init, { compress_experimental, compression_memory_plan } from './pkg/picoli_wasm.js';

let busy = false;
let approveMemory;

// Every compression gets a fresh worker, so terminating it releases the WASM heap.
self.onmessage = async ({ data }) => {
  if (data?.type === 'approve-memory') {
    if (data.approved === true && approveMemory) {
      approveMemory(); approveMemory = null;
    }
    return;
  }
  if (busy || data?.type !== 'compress' || !(data.input instanceof ArrayBuffer)) return;
  busy = true;
  try {
    const start = performance.now();
    let approvalWait = 0;
    self.postMessage({ type: 'phase', phase: 'loading' });
    await init();
    const input = new Uint8Array(data.input);
    let memoryMiB = data.memoryMiB;
    if (memoryMiB === 'auto') {
      self.postMessage({ type: 'phase', phase: 'planning' });
      const plan = JSON.parse(compression_memory_plan(input));
      if (plan.memory_mib >= 512) {
        const approval = new Promise(resolve => { approveMemory = resolve; });
        const waitingSince = performance.now();
        self.postMessage({ type: 'memory-plan', plan });
        await approval;
        approvalWait = performance.now() - waitingSince;
      } else {
        self.postMessage({ type: 'memory-plan', plan });
      }
      memoryMiB = plan.memory_mib;
    }
    self.postMessage({ type: 'phase', phase: 'compressing' });
    // Explicit budgets and omitted core API arguments retain their existing behavior.
    const encoded = compress_experimental(input, data.strength, memoryMiB);
    const buffer = encoded.byteOffset === 0 && encoded.byteLength === encoded.buffer.byteLength
      ? encoded.buffer : encoded.slice().buffer;
    self.postMessage({ type: 'result', buffer, memoryMiB, milliseconds: performance.now() - start - approvalWait }, [buffer]);
  } catch (error) {
    self.postMessage({ type: 'error', message: error instanceof Error ? error.message : String(error) });
  }
};
