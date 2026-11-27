// Execute the web-target bindings in Node; this is not a browser UI test.
import { readFileSync, writeFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { performance } from 'node:perf_hooks';
import { initSync, baseline_encode, baseline_capabilities } from './pkg/picoli_wasm.js';

const [sourcePath, nativeReportPath, outputPath] = process.argv.slice(2);
if (!sourcePath || !nativeReportPath || !outputPath) {
  throw new Error('Usage: node web/smoke.mjs INPUT NATIVE_REPORT NEW_OUTPUT_JSON');
}
const native = JSON.parse(readFileSync(nativeReportPath, 'utf8'));
const input = readFileSync(sourcePath);
const hash = (bytes) => createHash('sha256').update(bytes).digest('hex');
if (hash(input) !== native.source.sha256) throw new Error('Source hash mismatch');
const bytes = readFileSync(new URL('./pkg/picoli_wasm_bg.wasm', import.meta.url));
const { memory } = initSync({ module: bytes });
const checks = [];
for (const [format, quality, expected] of [
  ['png', 0, native.png_lossless.sha256],
  ...(native.selected_jpeg ? [['jpeg', native.selected_jpeg.quality, native.selected_jpeg.sha256]] : []),
]) {
  const start = performance.now();
  const encoded = baseline_encode(input, format, quality);
  const actual = hash(encoded);
  checks.push({ format, quality, bytes: encoded.length, sha256: actual,
    native_byte_identical: actual === expected, elapsed_ms: performance.now() - start });
  if (actual !== expected) throw new Error(`${format} differs between native and WASM`);
}
const report = {
  status: 'wasm_node_smoke_passed_not_browser_test',
  node: process.version,
  capabilities: JSON.parse(baseline_capabilities()),
  wasm_bytes: bytes.length,
  linear_memory_bytes_after_checks: memory.buffer.byteLength,
  memory_is_peak_rss_measurement: false,
  source_sha256: hash(input),
  checks,
};
writeFileSync(outputPath, JSON.stringify(report, null, 2), { flag: 'wx' });
console.log(JSON.stringify(report, null, 2));
