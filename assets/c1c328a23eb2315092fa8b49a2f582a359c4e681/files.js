export const MAX_INPUT_BYTES = 32 * 1024 * 1024;

/** Identify file contents rather than trusting an extension or browser MIME type. */
export async function inspectFile(file) {
  if (!file.size) throw new Error('图片文件是空的。');
  if (file.size > MAX_INPUT_BYTES) throw new Error('单张图片不能超过 32 MiB。');
  const bytes = new Uint8Array(await file.slice(0, 8).arrayBuffer());
  if ([137, 80, 78, 71, 13, 10, 26, 10].every((value, index) => bytes[index] === value)) {
    return { format: 'PNG', mime: 'image/png', extension: 'png' };
  }
  if (bytes[0] === 255 && bytes[1] === 216 && bytes[2] === 255) {
    return { format: 'JPG', mime: 'image/jpeg', extension: 'jpg' };
  }
  throw new Error('这里只支持 JPG 和 PNG 图片。');
}

export function formatBytes(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  const exponent = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), 3);
  return `${(bytes / (1024 ** exponent)).toFixed(exponent > 1 ? 2 : 1)} ${['B', 'KiB', 'MiB', 'GiB'][exponent]}`;
}

/** Keep UI errors readable while callers retain the original diagnostic. */
export function readableError(message) {
  if (/[㐀-鿿]/.test(message)) return message;
  if (/automatic.*budget.*1024|1024.*automatic.*budget/i.test(message)) return '这张图片所需的内存预算超过网页支持上限，请改用 CLI。';
  if (/budget/i.test(message)) return '当前内存预算不足，请选择“自动”或调高预算后重试，或改用 CLI。';
  if (/memory|allocation|out of memory/i.test(message)) return '设备可用内存不足，请关闭其他页面或改用 CLI，不要继续提高预算。';
  if (/unsupported|not supported|orientation|ICC|CMYK|bit.depth/i.test(message)) return '暂不支持这张图片的编码、色彩或方向配置。';
  if (/format error|decod|corrupt|truncat|unexpected|IHDR|invalid/i.test(message)) return '图片数据无法解码，文件可能损坏。';
  return '图片处理失败，请重试或换一张图片。';
}

export function formatDuration(milliseconds) {
  return milliseconds < 1000 ? `${Math.round(milliseconds)} ms` : `${(milliseconds / 1000).toFixed(2)} s`;
}

export function savingsPercent(before, after) {
  return before > 0 ? Math.max(0, (1 - after / before) * 100) : 0;
}

/** Output extension follows the actual format, not a potentially misleading name. */
export function outputName(name, extension) {
  const base = name.replace(/\.[^.]*$/, '').replace(/[<>:"/\\|?*\u0000-\u001f]/g, '_').trim() || 'image';
  return `${base}-picoli.${extension}`;
}

/** Ensure multiple files with the same name remain separate ZIP entries. */
export function uniqueNames(names) {
  const used = new Set();
  return names.map(name => {
    const index = name.lastIndexOf('.');
    const base = index > 0 ? name.slice(0, index) : name;
    const extension = index > 0 ? name.slice(index) : '';
    let candidate = name;
    let number = 2;
    while (used.has(candidate.toLowerCase())) candidate = `${base}-${number++}${extension}`;
    used.add(candidate.toLowerCase());
    return candidate;
  });
}
