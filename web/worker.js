// Minimal integration skeleton: baseline-only, no network upload or production claims.
import init, { baseline_encode, baseline_capabilities } from './pkg/picoli_wasm.js';

const ready = init();
self.onmessage = async ({ data }) => {
  const id = data?.id;
  try {
    await ready;
    if (data?.type === 'capabilities') {
      self.postMessage({ id, type: 'capabilities', value: JSON.parse(baseline_capabilities()) });
      return;
    }
    if (data?.type !== 'baseline-encode' || !(data.input instanceof ArrayBuffer)) {
      throw new Error('Expected baseline-encode with input ArrayBuffer');
    }
    const bytes = baseline_encode(new Uint8Array(data.input), data.format, data.quality ?? 80);
    self.postMessage({ id, type: 'baseline-result', productionEligible: false,
      format: data.format, output: bytes.buffer }, [bytes.buffer]);
  } catch (error) {
    self.postMessage({ id, type: 'error', message: String(error) });
  }
};
