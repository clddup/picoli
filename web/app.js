import { inspectFile, formatBytes, formatDuration, savingsPercent, outputName, readableError } from './files.js';
import { compressFile } from './engine-client.js';
import { createZip, downloadBlob } from './download.js';

const $ = id => document.getElementById(id);
const items = [];
let selectedId = null;
let running = false;
let currentAbort;
let runToken = 0;
let ingestionToken = 0;
let stopRequested = false;
let zoomed = false;
let toastTimer;
let packaging = false;

function toast(message) {
  clearTimeout(toastTimer);
  $('toast').textContent = message;
  $('toast').hidden = false;
  toastTimer = setTimeout(() => { $('toast').hidden = true; }, 5500);
}

function release(item) {
  if (item.sourceUrl) URL.revokeObjectURL(item.sourceUrl);
  if (item.resultUrl) URL.revokeObjectURL(item.resultUrl);
}

function select(id) {
  if (id !== selectedId) {
    zoomed = false;
    $('compare').value = '50';
  }
  selectedId = id;
  render();
}

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function iconButton(label, symbol, action) {
  const button = element('button', 'icon-button', symbol);
  button.type = 'button'; button.title = label; button.setAttribute('aria-label', label);
  button.addEventListener('click', action);
  return button;
}

function render() {
  document.body.dataset.processing = String(running);
  $('file-count').textContent = String(items.length);
  $('empty-queue').hidden = items.length !== 0;
  $('clear').disabled = items.length === 0;
  $('clear').textContent = running ? '停止并清空' : '清空';
  $('strength').disabled = running;
  $('reset-strength').disabled = running;
  $('start').disabled = !items.some(item => item.format) || packaging;
  $('start').textContent = running ? '停止处理' : items.some(item => item.status === 'done') ? '重新压缩 ↗' : '开始压缩 ↗';
  $('download-all').disabled = running || packaging || !items.some(item => item.blob);
  $('download-all').textContent = packaging ? '打包中…' : '下载 ZIP';
  const done = items.filter(item => item.status === 'done');
  const saved = done.reduce((sum, item) => sum + item.file.size - item.blob.size, 0);
  const active = items.find(item => item.status === 'running');
  $('queue-summary').textContent = active
    ? `${active.phase === 'loading' ? '加载压缩器' : '正在处理'} · ${active.file.name}`
    : running ? '正在准备下一张…'
    : done.length ? `已完成 ${done.length} 张${items.some(item => item.status === 'error') ? ` · ${items.filter(item => item.status === 'error').length} 张未完成` : ''} · 共减少 ${formatBytes(saved)}`
    : items.some(item => item.status === 'error') ? `${items.filter(item => item.status === 'error').length} 张未完成，可移除或重试`
    : items.some(item => item.status === 'cancelled') ? '已停止，可继续处理或清空队列'
    : items.length ? `${items.length} 张图片，等待处理` : '准备好，就开始。';

  const rows = items.map(item => {
    const row = element('div', `file-row${item.id === selectedId ? ' selected' : ''}`);
    row.dataset.id = item.id;
    const thumb = item.sourceUrl && !item.previewFailed ? element('img', 'file-thumb') : element('span', 'file-thumb', '—');
    if (item.sourceUrl && !item.previewFailed) {
      thumb.src = item.sourceUrl; thumb.alt = ''; thumb.loading = 'lazy';
      thumb.addEventListener('click', () => select(item.id));
    }
    const details = element('button', 'file-details'); details.type = 'button';
    details.title = `预览 ${item.file.name}`; details.setAttribute('aria-pressed', String(item.id === selectedId));
    details.addEventListener('click', () => select(item.id));
    details.append(element('span', 'file-title', item.file.name));
    const subtitle = element('span', 'file-subtitle');
    subtitle.append(element('span', '', formatBytes(item.file.size)));
    if (item.blob) {
      subtitle.append(element('span', 'arrow', '→'), element('span', 'file-result-size', formatBytes(item.blob.size)));
      const savedPercent = savingsPercent(item.file.size, item.blob.size);
      subtitle.append(element('span', `file-outcome${savedPercent === 0 ? ' unchanged' : ''}`, savedPercent > 0 ? `−${savedPercent.toFixed(1)}%` : '体积不变'));
    } else if (item.status === 'running') {
      subtitle.append(element('span', 'spinner'), element('span', '', item.phase === 'loading' ? '加载中' : '压缩中'));
    } else {
      const label = { ready: '等待处理', cancelled: '已停止', error: '未完成' }[item.status];
      subtitle.append(element('span', item.status === 'error' ? 'file-outcome error' : '', label));
    }
    details.append(subtitle);
    if (item.error) {
      const error = element('span', 'file-error', readableError(item.error));
      error.title = item.error; details.append(error);
    }
    const actions = element('div', 'file-actions');
    const download = iconButton(`下载 ${item.file.name}`, '↓', () => downloadBlob(item.blob, outputName(item.file.name, item.extension)));
    download.disabled = !item.blob;
    const remove = iconButton(`移除 ${item.file.name}`, '×', () => {
      const index = items.findIndex(value => value.id === item.id);
      if (index < 0) return;
      release(item); items.splice(index, 1);
      if (selectedId === item.id) { selectedId = items[0]?.id ?? null; zoomed = false; }
      render();
    });
    remove.disabled = item.status === 'running';
    actions.append(download, remove); row.append(thumb, details, actions);
    return row;
  });
  $('file-list').replaceChildren(...rows);
  renderPreview();
}

function renderPreview() {
  const item = items.find(value => value.id === selectedId);
  const visible = Boolean(item?.sourceUrl && !item.previewFailed);
  $('preview-empty').querySelector('h3').textContent = item?.previewFailed ? '这张图片无法预览。' : '图像对比视窗';
  $('preview-state').textContent = item?.previewFailed ? 'PREVIEW UNAVAILABLE' : 'AWAITING INPUT';
  $('preview-empty').querySelector('p').textContent = item?.previewFailed
    ? item.error ? readableError(item.error) : '文件可能损坏，仍可尝试压缩。'
    : '选中队列里的图片，在这里对比原图与压缩结果。';
  $('preview-empty').hidden = visible;
  $('preview-content').hidden = !visible;
  $('preview-format').textContent = item?.format ?? '预览';
  if (!visible) {
    $('original-image').removeAttribute('src'); $('result-image').removeAttribute('src');
    return;
  }
  $('preview-name').textContent = item.file.name;
  $('preview-name').title = item.file.name;
  if ($('original-image').getAttribute('src') !== item.sourceUrl) $('original-image').src = item.sourceUrl;
  if (item.resultUrl && $('result-image').getAttribute('src') !== item.resultUrl) $('result-image').src = item.resultUrl;
  if (!item.resultUrl) $('result-image').removeAttribute('src');
  $('result-image').hidden = !item.blob;
  $('split-line').hidden = !item.blob;
  $('compare-controls').hidden = !item.blob;
  $('preview-dimensions').textContent = item.dimensions ? `${item.dimensions.width} × ${item.dimensions.height} px` : item.previewFailed ? '浏览器无法预览' : '读取尺寸中';
  $('zoom').textContent = zoomed ? '原尺寸 100%' : '适应窗口';
  $('zoom').setAttribute('aria-pressed', String(zoomed));
  $('zoom').disabled = !item.dimensions;
  $('comparison').style.width = zoomed && item.dimensions ? `${item.dimensions.width}px` : '100%';
  $('comparison').style.height = zoomed && item.dimensions ? `${item.dimensions.height}px` : '100%';
  $('comparison').style.cursor = item.blob && !zoomed ? 'ew-resize' : 'auto';
  $('comparison').style.touchAction = item.blob && !zoomed ? 'pan-y' : 'auto';
  $('result-info').classList.toggle('error', item.status === 'error');
  $('result-info').title = item.error || '';
  if (item.blob) {
    const saving = savingsPercent(item.file.size, item.blob.size);
    $('result-info').replaceChildren(element('strong', '', saving > 0 ? `体积减少 ${saving.toFixed(1)}%` : '体积未减少'), element('span', '', `强度 ${item.usedStrength} · ${formatDuration(item.milliseconds)}`));
  } else {
    $('result-info').textContent = item.error ? readableError(item.error) : (item.status === 'running' ? '正在本地处理，页面可以继续操作。' : '压缩后可拖动分界，查看差异。');
    $('result-info').title = item.error || '';
  }
  updateSplit();
}

function updateSplit() {
  const value = Number($('compare').value);
  $('result-image').style.clipPath = `inset(0 ${100 - value}% 0 0)`;
  $('split-line').style.left = `${value}%`;
  $('compare').style.setProperty('--fill', `${value}%`);
}

async function addFiles(files) {
  const token = ingestionToken;
  let added = 0;
  for (const file of files) {
    if (token !== ingestionToken) break;
    const item = { id: crypto.randomUUID(), file, status: 'ready', sourceUrl: null, blob: null, resultUrl: null, error: '' };
    try {
      Object.assign(item, await inspectFile(file));
      if (token !== ingestionToken) break;
      item.sourceUrl = URL.createObjectURL(file);
    } catch (error) {
      if (token !== ingestionToken) break;
      item.status = 'error'; item.error = error.message;
    }
    items.push(item); added++;
    if (selectedId === null && item.sourceUrl) selectedId = item.id;
    if (item.sourceUrl) {
      const image = new Image();
      image.onload = () => {
        if (!items.includes(item)) return;
        item.dimensions = { width: image.naturalWidth, height: image.naturalHeight };
        renderPreview();
      };
      image.onerror = () => {
        if (!items.includes(item)) return;
        item.previewFailed = true;
        render();
      };
      image.src = item.sourceUrl;
    }
  }
  if (added) render();
  $('file-input').value = '';
}

async function start() {
  if (running) { stopRequested = true; currentAbort?.abort(); return; }
  const strength = Number($('strength').value);
  let pending = items.filter(item => item.format && (item.status !== 'done' || item.usedStrength !== strength));
  if (!pending.length) pending = items.filter(item => item.format);
  if (!pending.length) return;
  const token = ++runToken;
  stopRequested = false; running = true;
  render();
  try {
    for (const item of pending) {
      if (token !== runToken || stopRequested) break;
      if (!items.includes(item)) continue;
      if (item.resultUrl) URL.revokeObjectURL(item.resultUrl);
      item.blob = null; item.resultUrl = null; item.error = '';
      item.status = 'running'; item.phase = 'loading';
      currentAbort = new AbortController(); render();
      try {
        const result = await compressFile(item.file, strength, {
          signal: currentAbort.signal,
          onPhase: phase => { if (token === runToken && items.includes(item)) { item.phase = phase; render(); } },
        });
        if (token !== runToken || !items.includes(item)) break;
        item.blob = new Blob([result.buffer], { type: item.mime });
        item.resultUrl = URL.createObjectURL(item.blob);
        item.milliseconds = result.milliseconds; item.usedStrength = strength;
        item.status = 'done';
      } catch (error) {
        if (token !== runToken || !items.includes(item)) break;
        item.status = error.name === 'AbortError' ? 'cancelled' : 'error';
        item.error = error.name === 'AbortError' ? '' : error.message;
        if (error.name === 'AbortError') break;
      }
      render();
    }
  } finally {
    if (token === runToken) { running = false; currentAbort = null; render(); }
  }
}

$('drop-zone').addEventListener('click', () => $('file-input').click());
$('file-input').addEventListener('change', event => { void addFiles(Array.from(event.target.files)); });
window.addEventListener('dragover', event => {
  if (event.dataTransfer?.types.includes('Files')) { event.preventDefault(); $('drop-zone').classList.add('dragging'); }
});
window.addEventListener('dragleave', event => { if (!event.relatedTarget) $('drop-zone').classList.remove('dragging'); });
window.addEventListener('drop', event => {
  event.preventDefault(); $('drop-zone').classList.remove('dragging');
  if (event.dataTransfer?.files.length) void addFiles(Array.from(event.dataTransfer.files));
});
$('start').addEventListener('click', () => { void start(); });
$('clear').addEventListener('click', () => {
  ++runToken; ++ingestionToken; stopRequested = true; currentAbort?.abort(); currentAbort = null; running = false;
  items.forEach(release); items.length = 0; selectedId = null; zoomed = false; render();
});
function syncStrength() {
  $('strength-value').value = $('strength').value;
  $('strength').style.setProperty('--fill', `${$('strength').value}%`);
}
$('strength').addEventListener('input', syncStrength);
$('reset-strength').addEventListener('click', () => { $('strength').value = '50'; syncStrength(); });
const reducedMotion = matchMedia('(prefers-reduced-motion: reduce)');
$('drop-zone').addEventListener('pointermove', event => {
  if (reducedMotion.matches) return;
  const bounds = $('drop-zone').getBoundingClientRect();
  $('drop-zone').style.setProperty('--pointer-x', `${event.clientX - bounds.left}px`);
  $('drop-zone').style.setProperty('--pointer-y', `${event.clientY - bounds.top}px`);
});
$('drop-zone').addEventListener('pointerleave', () => {
  $('drop-zone').style.removeProperty('--pointer-x');
  $('drop-zone').style.removeProperty('--pointer-y');
});
$('compare').addEventListener('input', updateSplit);
let draggingComparison = false;
function dragComparison(event) {
  const rect = $('comparison').getBoundingClientRect();
  $('compare').value = String(Math.round(Math.max(0, Math.min(100, (event.clientX - rect.left) / rect.width * 100))));
  updateSplit();
}
$('comparison').addEventListener('pointerdown', event => {
  if (event.button !== 0 || zoomed || !items.find(item => item.id === selectedId)?.blob) return;
  draggingComparison = true;
  $('comparison').setPointerCapture(event.pointerId);
  dragComparison(event);
});
$('comparison').addEventListener('pointermove', event => { if (draggingComparison) dragComparison(event); });
for (const name of ['pointerup', 'pointercancel', 'lostpointercapture']) {
  $('comparison').addEventListener(name, () => { draggingComparison = false; });
}
$('zoom').addEventListener('click', () => { zoomed = !zoomed; renderPreview(); });
for (const button of document.querySelectorAll('[data-background]')) {
  button.addEventListener('click', () => {
    for (const swatch of document.querySelectorAll('[data-background]')) swatch.setAttribute('aria-pressed', String(swatch === button));
    $('preview-scroller').className = `preview-scroller ${button.dataset.background}`;
  });
}
$('download-all').addEventListener('click', async () => {
  if (packaging) return;
  const entries = items.filter(item => item.blob).map(item => ({ name: outputName(item.file.name, item.extension), blob: item.blob }));
  packaging = true; render();
  try { downloadBlob(await createZip(entries), 'picoli-images.zip'); }
  catch (error) { toast(error.message); }
  finally { packaging = false; render(); }
});
window.addEventListener('pagehide', () => { currentAbort?.abort(); });

render();
fetch(new URL('./version.json', import.meta.url), { cache: 'no-store' }).then(async response => {
  if (!response.ok) return;
  const version = await response.json();
  if (!/^[a-f0-9]{40}$/i.test(version.gitSha)) return;
  $('version-link').textContent = version.gitSha.slice(0, 7);
  $('version-link').href = `https://github.com/clddup/picoli/commit/${version.gitSha}`;
  $('version-link').title = `构建时间 ${version.buildTimestamp}`;
}).catch(() => { $('version-link').textContent = '本地预览'; });
