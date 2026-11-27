import { uniqueNames } from './files.js';

const crcTable = Uint32Array.from({ length: 256 }, (_, value) => {
  for (let bit = 0; bit < 8; bit++) value = (value >>> 1) ^ ((value & 1) ? 0xedb88320 : 0);
  return value >>> 0;
});

export function crc32(bytes) {
  let crc = 0xffffffff;
  for (const byte of bytes) crc = (crc >>> 8) ^ crcTable[(crc ^ byte) & 255];
  return (crc ^ 0xffffffff) >>> 0;
}

function header(size) {
  const data = new Uint8Array(size);
  const view = new DataView(data.buffer);
  return { data, u16: (offset, value) => view.setUint16(offset, value, true), u32: (offset, value) => view.setUint32(offset, value, true) };
}

/** Package encoded files without recompressing, renaming collisions or losing Unicode names. */
export async function createZip(entries) {
  if (!entries.length) throw new Error('还没有可下载的结果。');
  if (entries.length > 65535) throw new Error('单个 ZIP 最多支持 65535 个文件，请分批下载。');
  const names = uniqueNames(entries.map(entry => entry.name));
  const encoder = new TextEncoder();
  const localParts = [];
  const directoryParts = [];
  let offset = 0;
  let directorySize = 0;
  const now = new Date();
  const year = Math.max(1980, Math.min(2107, now.getFullYear()));
  const date = ((year - 1980) << 9) | ((now.getMonth() + 1) << 5) | now.getDate();
  const time = (now.getHours() << 11) | (now.getMinutes() << 5) | Math.floor(now.getSeconds() / 2);
  for (let index = 0; index < entries.length; index++) {
    const name = encoder.encode(names[index]);
    if (name.length > 65535) throw new Error('文件名太长，请单独下载或缩短文件名。');
    const blob = entries[index].blob;
    if (blob.size > 0xffffffff || offset + 30 + name.length + blob.size > 0xffffffff) {
      throw new Error('这批结果超出 ZIP 的 4 GiB 范围，请分批下载。');
    }
    const bytes = new Uint8Array(await blob.arrayBuffer());
    const crc = crc32(bytes);
    const local = header(30);
    local.u32(0, 0x04034b50); local.u16(4, 20); local.u16(6, 0x0800);
    local.u16(10, time); local.u16(12, date); local.u32(14, crc);
    local.u32(18, bytes.length); local.u32(22, bytes.length); local.u16(26, name.length);
    localParts.push(local.data, name, bytes);
    const central = header(46);
    central.u32(0, 0x02014b50); central.u16(4, 20); central.u16(6, 20); central.u16(8, 0x0800);
    central.u16(12, time); central.u16(14, date); central.u32(16, crc);
    central.u32(20, bytes.length); central.u32(24, bytes.length); central.u16(28, name.length); central.u32(42, offset);
    directoryParts.push(central.data, name);
    offset += 30 + name.length + bytes.length;
    directorySize += 46 + name.length;
  }
  if (offset + directorySize + 22 > 0xffffffff) throw new Error('这批结果超出 ZIP 的 4 GiB 范围，请分批下载。');
  const end = header(22);
  end.u32(0, 0x06054b50); end.u16(8, entries.length); end.u16(10, entries.length);
  end.u32(12, directorySize); end.u32(16, offset);
  return new Blob([...localParts, ...directoryParts, end.data], { type: 'application/zip' });
}

export function downloadBlob(blob, name) {
  const url = URL.createObjectURL(blob);
  const link = document.createElement('a');
  link.href = url; link.download = name;
  document.body.append(link); link.click(); link.remove();
  // Keep the URL alive long enough for browsers to start large downloads.
  setTimeout(() => URL.revokeObjectURL(url), 60000);
}
