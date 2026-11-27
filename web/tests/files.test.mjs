import test from 'node:test';
import assert from 'node:assert/strict';
import { inspectFile, MAX_INPUT_BYTES, formatBytes, outputName, uniqueNames, savingsPercent, readableError } from '../files.js';
import { createZip, crc32 } from '../download.js';

const png = new Uint8Array([137, 80, 78, 71, 13, 10, 26, 10]);

test('file type follows bytes, not extension or MIME', async () => {
  assert.equal((await inspectFile(new File([png], 'not-jpeg.jpg', { type: 'image/jpeg' }))).format, 'PNG');
  assert.equal((await inspectFile(new File([new Uint8Array([255, 216, 255, 224])], 'not-png.png'))).format, 'JPG');
  await assert.rejects(inspectFile(new File(['<svg></svg>'], 'image.png')), /JPG 和 PNG/);
});

test('empty and oversized inputs fail before allocation', async () => {
  await assert.rejects(inspectFile(new File([], 'empty.png')), /空/);
  await assert.rejects(inspectFile({ size: MAX_INPUT_BYTES + 1, slice() { throw new Error('must not read'); } }), /32 MiB/);
});

test('filenames remain safe and reflect real format', () => {
  assert.equal(outputName('照片.JPG', 'png'), '照片-picoli.png');
  assert.equal(outputName('../../bad/name.png', 'png'), '.._.._bad_name-picoli.png');
  assert.equal(outputName('.png', 'png'), 'image-picoli.png');
  const names = uniqueNames(['a.png', 'a.png', 'a-2.png', 'A.PNG']);
  assert.equal(new Set(names.map(name => name.toLowerCase())).size, 4);
});

test('byte and saving labels do not fabricate gains', () => {
  assert.equal(formatBytes(512), '512 B');
  assert.equal(formatBytes(1024), '1.0 KiB');
  assert.equal(savingsPercent(100, 100), 0);
  assert.equal(savingsPercent(100, 120), 0);
  assert.equal(savingsPercent(0, 0), 0);
  assert.ok(Math.abs(savingsPercent(100, 70) - 30) < 1e-12);
});

test('technical errors have readable UI descriptions without inventing success', () => {
  assert.match(readableError('Format error decoding Png: invalid IHDR'), /无法解码/);
  assert.match(readableError('memory budget exceeded'), /预算/);
  assert.match(readableError('memory budget exceeded'), /调高/);
  assert.match(readableError('allocation failed: out of memory'), /不要继续提高预算/);
  assert.match(readableError('unsupported ICC profile'), /色彩/);
  assert.equal(readableError('这里只支持 JPG 和 PNG 图片。'), '这里只支持 JPG 和 PNG 图片。');
});

test('CRC matches standard known vector', () => {
  assert.equal(crc32(new TextEncoder().encode('123456789')), 0xcbf43926);
});

test('ZIP preserves bytes, Unicode and duplicate file names', async () => {
  const bytes = new Uint8Array(await (await createZip([
    { name: '照片.png', blob: new Blob([png]) },
    { name: '照片.png', blob: new Blob(['hello']) },
  ])).arrayBuffer());
  const view = new DataView(bytes.buffer);
  const end = bytes.length - 22;
  assert.equal(view.getUint32(end, true), 0x06054b50);
  assert.equal(view.getUint16(end + 10, true), 2);
  let offset = view.getUint32(end + 16, true);
  for (const [index, expected] of ['照片.png', '照片-2.png'].entries()) {
    assert.equal(view.getUint32(offset, true), 0x02014b50);
    assert.equal(view.getUint16(offset + 8, true), 0x0800);
    const nameLength = view.getUint16(offset + 28, true);
    assert.equal(new TextDecoder().decode(bytes.slice(offset + 46, offset + 46 + nameLength)), expected);
    const local = view.getUint32(offset + 42, true);
    assert.equal(view.getUint32(local, true), 0x04034b50);
    const length = view.getUint32(local + 18, true);
    const data = bytes.slice(local + 30 + nameLength, local + 30 + nameLength + length);
    assert.deepEqual(data, index ? new TextEncoder().encode('hello') : png);
    assert.equal(view.getUint32(local + 14, true), crc32(data));
    offset += 46 + nameLength;
  }
  assert.equal(offset, end);
});

test('ZIP rejects empty batches and ZIP64-sized payloads', async () => {
  await assert.rejects(createZip([]), /没有/);
  await assert.rejects(createZip([{ name: 'huge.png', blob: { size: 0x100000000 } }]), /4 GiB/);
});
