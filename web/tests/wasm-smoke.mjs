// Execute the actual built web-target WASM in Node (not a browser test).
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { deflateSync } from 'node:zlib';
import { initSync, compress_experimental, baseline_capabilities } from '../pkg/picoli_wasm.js';
import { crc32 } from '../download.js';

function chunk(type, data) {
  const name = Buffer.from(type);
  const result = Buffer.alloc(12 + data.length);
  result.writeUInt32BE(data.length);
  name.copy(result, 4); data.copy(result, 8);
  result.writeUInt32BE(crc32(Buffer.concat([name, data])), 8 + data.length);
  return result;
}
const header = Buffer.alloc(13);
header.writeUInt32BE(64, 0); header.writeUInt32BE(64, 4); header[8] = 8; header[9] = 6;
const rows = [];
for (let y = 0; y < 64; y++) {
  const row = Buffer.alloc(1 + 64 * 4);
  for (let x = 0; x < 64; x++) {
    const offset = 1 + x * 4;
    row[offset] = x < 32 ? 50 : 200;
    row[offset + 1] = y < 32 ? 80 : 180;
    row[offset + 2] = 120;
    row[offset + 3] = (x + y) % 8 === 0 ? 0 : 255;
  }
  rows.push(row);
}
const input = Buffer.concat([Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]), chunk('IHDR', header), chunk('IDAT', deflateSync(Buffer.concat(rows), { level: 0 })), chunk('IEND', Buffer.alloc(0))]);
initSync({ module: readFileSync(new URL('../pkg/picoli_wasm_bg.wasm', import.meta.url)) });
assert.deepEqual(Buffer.from(compress_experimental(input, 0)), input);
const result = Buffer.from(compress_experimental(input));
assert.deepEqual(result, Buffer.from(compress_experimental(input, 50, 128)));
assert.ok(result.length <= input.length);
assert.deepEqual(result.subarray(0, 8), input.subarray(0, 8));
assert.equal(result.readUInt32BE(16), 64); assert.equal(result.readUInt32BE(20), 64);
assert.equal(JSON.parse(baseline_capabilities()).default_strength_percent, 50);
assert.throws(() => compress_experimental(new Uint8Array([0, 1, 2])));
console.log(`Actual WASM smoke passed: ${input.length} -> ${result.length} B; zero-strength byte-exact; defaults unchanged.`);
